//! Materialization cache for CAS checkout.
//!
//! Maintains a cache directory where materialized project snapshots
//! are stored. Skips re-materialization if the cache already has the
//! matching project snapshot hash.

#[cfg(unix)]
use std::collections::HashMap;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs;
use std::io::{Read as _, Seek as _};
#[cfg(unix)]
use std::os::fd::AsRawFd as _;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::sync::{Mutex, OnceLock};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

const COMPLETION_MARKER_KIND: &str = "ryeos_materialization_cache_generation";
const COMPLETION_MARKER_SCHEMA: u32 = 1;
const MAX_COMPLETION_MARKER_BYTES: u64 = 128 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CachedTreeEntry {
    blob_hash: String,
    size: u64,
    normalized_mode: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CompletionMarker {
    kind: String,
    schema: u32,
    snapshot_hash: String,
    tree_hash: String,
    directories: BTreeSet<String>,
    files: BTreeMap<String, CachedTreeEntry>,
}

#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct FileIdentity {
    device: u64,
    inode: u64,
    size: u64,
    mode: u32,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
}

#[cfg(unix)]
static VERIFIED_FILE_IDENTITIES: OnceLock<Mutex<HashMap<FileIdentity, String>>> = OnceLock::new();

/// Materialization cache backed by a directory on disk.
///
/// Layout: `{cache_root}/{snapshot_hash}/` contains the materialized files.
pub struct MaterializationCache {
    cache_root: PathBuf,
}

/// Exact outcome of one lease-aware materialization-cache sweep.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MaterializationPruneReport {
    /// Complete generations inspected by the sweep.
    pub inspected_generations: usize,
    /// Inactive generations eligible for removal. In a dry run these are the
    /// generations that would be removed.
    pub reclaimable_generations: usize,
    /// Generations preserved because a builder or workspace holds the exact
    /// construction/lease lock.
    pub active_generations: usize,
    /// Files removed, or files that would be removed during a dry run.
    pub reclaimable_files: usize,
    /// Logical bytes removed, or logical bytes that would be removed during a
    /// dry run. Shared content-cache inodes are counted only when unlinked.
    pub reclaimable_bytes: u64,
}

/// One content-cache inode kept open from verification through publication
/// into a lower tree. Linking uses this descriptor, never a second path walk.
pub struct VerifiedContentFile {
    file: fs::File,
    parent: lillux::secure_fs::PinnedDirectory,
    name: std::ffi::OsString,
    path: PathBuf,
    _build_lock: fs::File,
}

impl VerifiedContentFile {
    pub fn link_to(
        &self,
        destination_parent: &lillux::secure_fs::PinnedDirectory,
        destination_name: &OsStr,
    ) -> Result<()> {
        destination_parent
            .link_regular_from(destination_name, &self.parent, &self.name, &self.file)
            .with_context(|| {
                format!(
                    "link verified cache inode {} to {}",
                    self.path.display(),
                    destination_parent.path().join(destination_name).display()
                )
            })
    }
}

impl MaterializationCache {
    pub fn new(cache_root: PathBuf) -> Self {
        Self { cache_root }
    }

    /// Resolve the materialization cache beneath the runtime cache root. This
    /// keeps ownership of the on-disk layout in the executor layer.
    pub fn from_runtime_cache(runtime_cache_root: &Path) -> Self {
        Self::new(runtime_cache_root.join("snapshots"))
    }

    /// Resolve the materialization cache beneath a runtime-state root.
    pub fn from_runtime_state(runtime_state_root: &Path) -> Self {
        Self::from_runtime_cache(&runtime_state_root.join("cache"))
    }

    /// Check if a project snapshot hash is already cached.
    pub fn _has(&self, snapshot_hash: &str) -> bool {
        self.cache_dir(snapshot_hash).is_dir()
    }

    /// Get the cache directory for a project snapshot hash.
    pub fn cache_dir(&self, snapshot_hash: &str) -> PathBuf {
        self.cache_root.join(snapshot_hash)
    }

    fn content_path(&self, file: &ryeos_state::objects::ProjectFile) -> PathBuf {
        self.cache_root
            .join(".files")
            .join("v1")
            .join(format!("{:04o}", file.normalized_mode))
            .join(&file.blob_hash[..2])
            .join(&file.blob_hash[2..4])
            .join(&file.blob_hash)
    }

    /// Return the one verified immutable cache inode for a blob+mode key.
    pub fn ensure_content_file(
        &self,
        cas: &lillux::cas::CasStore,
        file: &ryeos_state::objects::ProjectFile,
    ) -> Result<VerifiedContentFile> {
        file.validate()?;
        let target = self.content_path(file);
        let parent = target
            .parent()
            .ok_or_else(|| anyhow::anyhow!("content cache path has no parent"))?;
        let parent = lillux::PinnedDirectory::open_or_create(parent)?;
        let target_name = target
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("content cache path has no filename"))?;
        let build_lock =
            parent.open_regular_create(OsStr::new(".build.lock"), true, false, 0o600)?;
        #[cfg(unix)]
        if unsafe { libc::flock(build_lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // Every reused inode is content-verified once per process. The cached
        // identity includes ctime as well as inode, size, mode, and mtime, so
        // an in-place mutation invalidates the fast path even if the writer
        // restores ordinary file metadata.
        if let Some(existing) = parent.open_regular(target_name, false)? {
            let verified = verify_opened_cached_file(existing, &target, file)?;
            return Ok(VerifiedContentFile {
                file: verified,
                parent,
                name: target_name.to_os_string(),
                path: target,
                _build_lock: build_lock,
            });
        }
        let staging_name = std::ffi::OsString::from(format!(
            ".{}.staging.{}.{}",
            file.blob_hash,
            std::process::id(),
            rand::random::<u64>()
        ));
        let size = match cas.materialize_blob_to_new_regular(
            &file.blob_hash,
            &parent,
            &staging_name,
            file.normalized_mode,
        ) {
            Ok(size) => size,
            Err(error) => {
                if let Some(staged) = parent.open_regular(&staging_name, false)? {
                    parent.remove_if_same(&staging_name, &staged)?;
                }
                return Err(error);
            }
        };
        let staging_file = parent
            .open_regular(&staging_name, false)?
            .ok_or_else(|| anyhow::anyhow!("content-cache staging file disappeared"))?;
        if size != file.size {
            parent.remove_if_same(&staging_name, &staging_file)?;
            anyhow::bail!(
                "cached project blob {} has size {}, expected {}",
                file.blob_hash,
                size,
                file.size
            );
        }
        match parent.publish_regular_link_from(target_name, &parent, &staging_name, &staging_file) {
            Ok(true) => {
                let target_file = parent
                    .open_regular(target_name, false)?
                    .ok_or_else(|| anyhow::anyhow!("published content-cache file disappeared"))?;
                verify_same_file(&staging_file, &target_file)?;
                remember_verified_descriptor(&target_file, file)?;
                return Ok(VerifiedContentFile {
                    file: target_file,
                    parent,
                    name: target_name.to_os_string(),
                    path: target,
                    _build_lock: build_lock,
                });
            }
            Ok(false) => {}
            Err(error) => {
                parent.remove_if_same(&staging_name, &staging_file)?;
                return Err(error);
            }
        }
        parent.remove_if_same(&staging_name, &staging_file)?;
        let existing = parent
            .open_regular(target_name, false)?
            .ok_or_else(|| anyhow::anyhow!("winning content-cache file disappeared"))?;
        let verified = verify_opened_cached_file(existing, &target, file)?;
        Ok(VerifiedContentFile {
            file: verified,
            parent,
            name: target_name.to_os_string(),
            path: target,
            _build_lock: build_lock,
        })
    }

    pub fn pinned_root(&self) -> Result<lillux::PinnedDirectory> {
        lillux::PinnedDirectory::open_or_create(&self.cache_root)
    }

    /// Publish a complete snapshot tree without replacing a winning builder.
    /// The cache parent and staging directory are the exact descriptors used
    /// to construct the generation; publication never reopens either path.
    pub fn publish_tree(
        &self,
        cache_root: &lillux::PinnedDirectory,
        staging_name: &OsStr,
        staging: &lillux::PinnedDirectory,
        snapshot_hash: &str,
    ) -> Result<()> {
        validate_canonical_hash("materialization snapshot hash", snapshot_hash)?;
        let completion = inspect_pinned_tree(staging, snapshot_hash)?;
        staging.sync_tree()?;
        let final_name = OsStr::new(snapshot_hash);
        match cache_root.rename_child_directory_noreplace(staging_name, final_name, staging) {
            Ok(()) => self.write_completion_marker(&completion),
            Err(error) => {
                if !error.namespace_committed() && self.verify_complete(snapshot_hash).is_ok() {
                    staging.remove_contents_recursive()?;
                    if !cache_root.remove_empty_child_if_same(staging_name, staging)? {
                        anyhow::bail!("materialization staging directory remained non-empty");
                    }
                    Ok(())
                } else {
                    Err(error.into())
                }
            }
        }
    }

    /// Acquire the mandatory shared lease for one exact snapshot generation.
    /// The caller retains the returned descriptor for the workspace lifetime.
    pub fn generation_lease(&self, snapshot_hash: &str) -> Result<fs::File> {
        validate_canonical_hash("materialization snapshot hash", snapshot_hash)?;
        let leases = self.cache_root.join(".leases");
        fs::create_dir_all(&leases)?;
        let path = leases.join(snapshot_hash);
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        #[cfg(unix)]
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(file)
    }

    pub fn generation_build_lock(&self, snapshot_hash: &str) -> Result<fs::File> {
        validate_canonical_hash("materialization snapshot hash", snapshot_hash)?;
        let locks = self.cache_root.join(".locks");
        fs::create_dir_all(&locks)?;
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(locks.join(snapshot_hash))?;
        #[cfg(unix)]
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(file)
    }

    fn try_generation_build_lock(&self, snapshot_hash: &str) -> Result<Option<fs::File>> {
        validate_canonical_hash("materialization snapshot hash", snapshot_hash)?;
        let locks = self.cache_root.join(".locks");
        fs::create_dir_all(&locks)?;
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(locks.join(snapshot_hash))?;
        #[cfg(unix)]
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::WouldBlock {
                return Ok(None);
            }
            return Err(error.into());
        }
        Ok(Some(file))
    }

    pub fn remove_incomplete_tree(&self, snapshot_hash: &str) -> Result<()> {
        validate_canonical_hash("materialization snapshot hash", snapshot_hash)?;
        if self.verify_complete(snapshot_hash).is_err() {
            remove_path_no_follow(&self.cache_dir(snapshot_hash))?;
            remove_file_if_present(&self.cache_root.join(".complete").join(snapshot_hash))?;
        }
        Ok(())
    }

    /// Record that a snapshot generation has been materialized to a directory.
    ///
    /// Creates an out-of-tree marker so cache metadata can never become part
    /// of a resolved or executed project generation.
    pub fn mark_complete(&self, snapshot_hash: &str) -> Result<()> {
        let dir = self.cache_dir(snapshot_hash);
        validate_canonical_hash("materialization snapshot hash", snapshot_hash)?;
        let directory = lillux::PinnedDirectory::open(&dir)?.ok_or_else(|| {
            anyhow::anyhow!(
                "materialization cache generation is missing or is not a directory: {}",
                dir.display()
            )
        })?;
        let completion = inspect_pinned_tree(&directory, snapshot_hash)?;
        directory.sync_tree()?;
        self.write_completion_marker(&completion)
    }

    fn write_completion_marker(&self, completion: &CompletionMarker) -> Result<()> {
        let markers = self.cache_root.join(".complete");
        let markers = lillux::PinnedDirectory::open_or_create(&markers)?;
        let bytes = canonical_completion_marker(completion)?.into_bytes();
        let name = OsStr::new(&completion.snapshot_hash);
        if markers
            .atomic_create_regular(name, &bytes, 0o600)?
            .is_none()
        {
            let existing = markers
                .open_regular(name, false)?
                .ok_or_else(|| anyhow::anyhow!("completion marker winner disappeared"))?;
            let observed = read_completion_marker_file(
                existing,
                &markers.path().join(name),
                &completion.snapshot_hash,
            )?;
            if observed != *completion {
                anyhow::bail!(
                    "existing completion marker contradicts snapshot {}",
                    completion.snapshot_hash
                );
            }
        }
        Ok(())
    }

    /// Check if a cache entry is fully materialized (not partial).
    pub fn is_complete(&self, snapshot_hash: &str) -> bool {
        self.verify_complete(snapshot_hash).is_ok()
    }

    fn verify_complete(&self, snapshot_hash: &str) -> Result<()> {
        validate_canonical_hash("materialization snapshot hash", snapshot_hash)?;
        let expected = read_completion_marker(
            &self.cache_root.join(".complete").join(snapshot_hash),
            snapshot_hash,
        )?;
        let observed = inspect_tree(&self.cache_dir(snapshot_hash), snapshot_hash)?;
        if observed != expected {
            anyhow::bail!(
                "materialization cache generation {} does not match its completion marker",
                snapshot_hash
            );
        }
        Ok(())
    }

    /// Bind a cached generation to the immutable CAS ProjectTree requested by
    /// the caller. The adjacent marker is only an acceleration/integrity
    /// record; it is never the authority for expected paths or file metadata.
    pub fn verify_complete_for_tree(
        &self,
        cas: &lillux::cas::CasStore,
        tree: &ryeos_state::objects::ProjectTree,
        snapshot_hash: &str,
    ) -> Result<()> {
        let expected = expected_completion_marker(cas, tree, snapshot_hash)?;
        let marker = read_completion_marker(
            &self.cache_root.join(".complete").join(snapshot_hash),
            snapshot_hash,
        )?;
        if marker != expected {
            anyhow::bail!(
                "materialization marker for {snapshot_hash} contradicts the authoritative CAS tree"
            );
        }
        let observed = inspect_tree(&self.cache_dir(snapshot_hash), snapshot_hash)?;
        if observed != expected {
            anyhow::bail!(
                "materialized generation {snapshot_hash} contradicts the authoritative CAS tree"
            );
        }
        Ok(())
    }

    pub fn discard_generation(&self, snapshot_hash: &str) -> Result<()> {
        validate_canonical_hash("materialization snapshot hash", snapshot_hash)?;
        remove_path_no_follow(&self.cache_dir(snapshot_hash))?;
        remove_file_if_present(&self.cache_root.join(".complete").join(snapshot_hash))
    }

    /// Evict a cache entry.
    pub fn _evict(&self, snapshot_hash: &str) -> Result<bool> {
        Ok(self.evict_with_footprint(snapshot_hash)?.is_some())
    }

    fn evict_with_footprint(&self, snapshot_hash: &str) -> Result<Option<(usize, u64)>> {
        validate_canonical_hash("materialization snapshot hash", snapshot_hash)?;
        // Construction lock precedes the exact generation lease everywhere.
        // A checkout retains this lock until it has acquired its shared lease,
        // so eviction cannot remove a tree in the link-to-lower gap.
        let Some(_construction) = self.try_generation_build_lock(snapshot_hash)? else {
            return Ok(None);
        };
        let leases = self.cache_root.join(".leases");
        fs::create_dir_all(&leases)?;
        let lease = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(leases.join(snapshot_hash))?;
        #[cfg(unix)]
        if unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::WouldBlock {
                return Ok(None);
            }
            return Err(error.into());
        }
        let dir = self.cache_dir(snapshot_hash);
        match fs::symlink_metadata(&dir) {
            Ok(_) => {
                let (files, bytes) = path_footprint(&dir)?;
                let (marker_files, marker_bytes) =
                    path_footprint(&self.cache_root.join(".complete").join(snapshot_hash))?;
                remove_path_no_follow(&dir)?;
                let marker = self.cache_root.join(".complete").join(snapshot_hash);
                remove_file_if_present(&marker)?;
                Ok(Some((
                    files + marker_files,
                    bytes.saturating_add(marker_bytes),
                )))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    /// Bound immutable generation metadata and unlink content inodes no tree
    /// or workspace references. Active generations are skipped through exact
    /// non-blocking leases; cleanup never creates writable aliases.
    pub fn prune(&self, max_generations: usize) -> Result<()> {
        self.prune_abandoned_staging()?;
        let mut generations = self._list()?;
        if generations.len() > max_generations {
            generations.sort_by_key(|hash| {
                fs::metadata(self.cache_dir(hash))
                    .and_then(|metadata| metadata.modified())
                    .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
            });
            let remove = generations.len().saturating_sub(max_generations);
            for hash in generations.into_iter().take(remove) {
                let _ = self._evict(&hash)?;
            }
        }
        self.prune_unlinked_content()
    }

    /// Reclaim every inactive immutable generation.
    ///
    /// A generation is disposable cache, but only after the executor acquires
    /// its construction lock and exact lease exclusively. Active builders and
    /// workspaces are therefore preserved without relying on process-local
    /// bookkeeping. Dry-run performs no filesystem mutations, including lock
    /// anchor creation.
    pub fn prune_inactive_generations(&self, dry_run: bool) -> Result<MaterializationPruneReport> {
        if !self.cache_root.is_dir() {
            return Ok(MaterializationPruneReport::default());
        }
        if !dry_run {
            self.prune_abandoned_staging()?;
        }

        let mut report = MaterializationPruneReport::default();
        for hash in self._list()? {
            validate_canonical_hash("materialization snapshot hash", &hash)?;
            report.inspected_generations += 1;
            let active = if dry_run {
                self.generation_has_live_lock(&hash)?
            } else {
                false
            };
            if active {
                report.active_generations += 1;
                continue;
            }

            let reclaimed = if dry_run {
                let (files, bytes) = path_footprint(&self.cache_dir(&hash))?;
                let (marker_files, marker_bytes) =
                    path_footprint(&self.cache_root.join(".complete").join(&hash))?;
                Some((files + marker_files, bytes.saturating_add(marker_bytes)))
            } else {
                self.evict_with_footprint(&hash)?
            };
            if let Some((files, bytes)) = reclaimed {
                report.reclaimable_generations += 1;
                report.reclaimable_files += files;
                report.reclaimable_bytes = report.reclaimable_bytes.saturating_add(bytes);
            } else {
                report.active_generations += 1;
            }
        }

        let (content_files, content_bytes) = self.prune_unlinked_content_inner(dry_run)?;
        report.reclaimable_files += content_files;
        report.reclaimable_bytes = report.reclaimable_bytes.saturating_add(content_bytes);
        Ok(report)
    }

    fn generation_has_live_lock(&self, snapshot_hash: &str) -> Result<bool> {
        validate_canonical_hash("materialization snapshot hash", snapshot_hash)?;
        if matches!(
            try_existing_exclusive_lock(&self.cache_root.join(".locks").join(snapshot_hash))?,
            ExistingLock::Busy
        ) {
            return Ok(true);
        }
        Ok(matches!(
            try_existing_exclusive_lock(&self.cache_root.join(".leases").join(snapshot_hash))?,
            ExistingLock::Busy
        ))
    }

    fn prune_abandoned_staging(&self) -> Result<()> {
        if !self.cache_root.is_dir() {
            return Ok(());
        }
        for entry in fs::read_dir(&self.cache_root)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Some(snapshot_hash) = staging_snapshot_hash(&name, false) else {
                continue;
            };
            // Never wait for or race an active builder during maintenance.
            // All generation staging is created while this lock is held.
            let Some(_construction) = self.try_generation_build_lock(snapshot_hash)? else {
                continue;
            };
            remove_path_no_follow(&entry.path())?;
        }

        let markers = self.cache_root.join(".complete");
        if markers.is_dir() {
            for entry in fs::read_dir(markers)? {
                let entry = entry?;
                if !entry.file_type()?.is_file() {
                    continue;
                }
                let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                    continue;
                };
                let Some(snapshot_hash) = staging_snapshot_hash(&name, true) else {
                    continue;
                };
                let Some(_construction) = self.try_generation_build_lock(snapshot_hash)? else {
                    continue;
                };
                remove_file_if_present(&entry.path())?;
            }
        }
        Ok(())
    }

    fn prune_unlinked_content(&self) -> Result<()> {
        self.prune_unlinked_content_inner(false).map(|_| ())
    }

    fn prune_unlinked_content_inner(&self, dry_run: bool) -> Result<(usize, u64)> {
        let content_root = self.cache_root.join(".files").join("v1");
        if !content_root.is_dir() {
            return Ok((0, 0));
        }
        let mut removed_files = 0usize;
        let mut removed_bytes = 0u64;
        for mode in fs::read_dir(&content_root)? {
            let mode = mode?;
            if !mode.file_type()?.is_dir() {
                anyhow::bail!("immutable content cache contains an unexpected mode entry");
            }
            for first in fs::read_dir(mode.path())? {
                let first = first?;
                if !first.file_type()?.is_dir() {
                    anyhow::bail!("immutable content cache contains an unexpected shard entry");
                }
                for second in fs::read_dir(first.path())? {
                    let second = second?;
                    if !second.file_type()?.is_dir() {
                        anyhow::bail!(
                            "immutable content cache contains an unexpected leaf shard entry"
                        );
                    }
                    let lock_path = second.path().join(".build.lock");
                    let _lock = if dry_run {
                        match try_existing_exclusive_lock(&lock_path)? {
                            ExistingLock::Busy => continue,
                            ExistingLock::Absent => None,
                            ExistingLock::Acquired(file) => Some(file),
                        }
                    } else {
                        let file = fs::OpenOptions::new()
                            .read(true)
                            .write(true)
                            .create(true)
                            .truncate(false)
                            .open(&lock_path)?;
                        #[cfg(unix)]
                        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }
                            != 0
                        {
                            let error = std::io::Error::last_os_error();
                            if error.kind() == std::io::ErrorKind::WouldBlock {
                                continue;
                            }
                            return Err(error.into());
                        }
                        Some(file)
                    };
                    for entry in fs::read_dir(second.path())? {
                        let entry = entry?;
                        if entry.file_name() == ".build.lock" {
                            continue;
                        }
                        let metadata = fs::symlink_metadata(entry.path())?;
                        if !metadata.file_type().is_file() {
                            anyhow::bail!(
                                "immutable content cache contains an unsupported entry: {}",
                                entry.path().display()
                            );
                        }
                        #[cfg(unix)]
                        {
                            use std::os::unix::fs::MetadataExt as _;
                            if metadata.nlink() == 1 {
                                removed_files += 1;
                                removed_bytes = removed_bytes.saturating_add(metadata.len());
                                if !dry_run {
                                    fs::remove_file(entry.path())?;
                                }
                            }
                        }
                    }
                }
            }
        }
        Ok((removed_files, removed_bytes))
    }

    /// List all cached snapshot hashes.
    pub fn _list(&self) -> Result<Vec<String>> {
        if !self.cache_root.is_dir() {
            return Ok(Vec::new());
        }
        let mut entries = Vec::new();
        for entry in fs::read_dir(&self.cache_root)? {
            let entry = entry?;
            if entry.path().is_dir()
                && !matches!(
                    entry.file_name().to_str(),
                    Some(".complete" | ".files" | ".leases" | ".locks")
                )
            {
                if let Some(name) = entry.file_name().to_str() {
                    if !name.contains(".staging.") {
                        entries.push(name.to_string());
                    }
                }
            }
        }
        Ok(entries)
    }
}

enum ExistingLock {
    Absent,
    Acquired(fs::File),
    Busy,
}

fn try_existing_exclusive_lock(path: &Path) -> Result<ExistingLock> {
    let file = match fs::OpenOptions::new().read(true).write(true).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ExistingLock::Absent);
        }
        Err(error) => return Err(error.into()),
    };
    #[cfg(unix)]
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::WouldBlock {
            return Ok(ExistingLock::Busy);
        }
        return Err(error.into());
    }
    Ok(ExistingLock::Acquired(file))
}

fn path_footprint(path: &Path) -> Result<(usize, u64)> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok((0, 0)),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() {
        anyhow::bail!(
            "materialization cache contains an unsupported symlink: {}",
            path.display()
        );
    }
    if metadata.is_file() {
        return Ok((1, metadata.len()));
    }
    if !metadata.is_dir() {
        anyhow::bail!(
            "materialization cache contains an unsupported entry: {}",
            path.display()
        );
    }
    let mut files = 0usize;
    let mut bytes = 0u64;
    for entry in fs::read_dir(path)? {
        let (entry_files, entry_bytes) = path_footprint(&entry?.path())?;
        files += entry_files;
        bytes = bytes.saturating_add(entry_bytes);
    }
    Ok((files, bytes))
}

fn validate_canonical_hash(label: &str, value: &str) -> Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        anyhow::bail!("{label} must be a lowercase SHA-256 hash, got {value:?}");
    }
    Ok(())
}

fn canonical_completion_marker(marker: &CompletionMarker) -> Result<String> {
    let value = serde_json::to_value(marker)?;
    lillux::canonical_json(&value).map_err(Into::into)
}

fn cached_tree_hash(
    directories: &BTreeSet<String>,
    files: &BTreeMap<String, CachedTreeEntry>,
) -> Result<String> {
    let value = serde_json::json!({
        "directories": directories,
        "files": files,
    });
    let canonical = lillux::canonical_json(&value)?;
    Ok(lillux::sha256_hex(canonical.as_bytes()))
}

fn inspect_tree(root: &Path, snapshot_hash: &str) -> Result<CompletionMarker> {
    let directory = lillux::PinnedDirectory::open(root)?.ok_or_else(|| {
        anyhow::anyhow!(
            "materialization cache generation is missing or is not a directory: {}",
            root.display()
        )
    })?;
    inspect_pinned_tree(&directory, snapshot_hash)
}

fn inspect_pinned_tree(
    root: &lillux::PinnedDirectory,
    snapshot_hash: &str,
) -> Result<CompletionMarker> {
    let mut directories = BTreeSet::new();
    let mut files = BTreeMap::new();
    root.visit_regular_files(
        |relative, directory| {
            if directory {
                let path = relative.to_str().ok_or_else(|| {
                    anyhow::anyhow!(
                        "materialization cache contains a non-UTF-8 path: {}",
                        relative.display()
                    )
                })?;
                ryeos_state::project_sync::validate_safe_relative_path(path)?;
                if !directories.insert(path.to_owned()) {
                    anyhow::bail!("materialization cache contains duplicate directory {path}");
                }
            }
            Ok(false)
        },
        |relative, mut file| {
            let path = relative.to_str().ok_or_else(|| {
                anyhow::anyhow!(
                    "materialization cache contains a non-UTF-8 path: {}",
                    relative.display()
                )
            })?;
            ryeos_state::project_sync::validate_safe_relative_path(path)?;
            let (blob_hash, metadata) = digest_verified_identity(&mut file)?;
            #[cfg(unix)]
            let normalized_mode = {
                use std::os::unix::fs::PermissionsExt as _;
                metadata.permissions().mode() & 0o777
            };
            #[cfg(not(unix))]
            let normalized_mode = ryeos_state::objects::ProjectFile::REGULAR_MODE;
            if !matches!(
                normalized_mode,
                ryeos_state::objects::ProjectFile::REGULAR_MODE
                    | ryeos_state::objects::ProjectFile::EXECUTABLE_MODE
            ) {
                anyhow::bail!(
                    "materialization cache entry has non-canonical mode {normalized_mode:#o}: {}",
                    root.path().join(relative).display()
                );
            }
            let entry = CachedTreeEntry {
                blob_hash,
                size: metadata.len(),
                normalized_mode,
            };
            if files.insert(path.to_owned(), entry).is_some() {
                anyhow::bail!("materialization cache contains duplicate path {path}");
            }
            Ok(())
        },
    )?;
    Ok(CompletionMarker {
        kind: COMPLETION_MARKER_KIND.to_owned(),
        schema: COMPLETION_MARKER_SCHEMA,
        snapshot_hash: snapshot_hash.to_owned(),
        tree_hash: cached_tree_hash(&directories, &files)?,
        directories,
        files,
    })
}

fn expected_completion_marker(
    cas: &lillux::cas::CasStore,
    tree: &ryeos_state::objects::ProjectTree,
    snapshot_hash: &str,
) -> Result<CompletionMarker> {
    validate_canonical_hash("materialization snapshot hash", snapshot_hash)?;
    let mut directories = BTreeSet::new();
    let mut files = BTreeMap::new();
    for (relative, object_hash) in &tree.files {
        ryeos_state::project_sync::validate_safe_relative_path(relative)?;
        let value = cas
            .get_object(object_hash)?
            .ok_or_else(|| anyhow::anyhow!("project file object {object_hash} is absent"))?;
        let file = ryeos_state::objects::ProjectFile::from_value(&value)?;
        let mut parent = Path::new(relative).parent();
        while let Some(path) = parent {
            if path.as_os_str().is_empty() {
                break;
            }
            directories.insert(path.to_string_lossy().into_owned());
            parent = path.parent();
        }
        files.insert(
            relative.clone(),
            CachedTreeEntry {
                blob_hash: file.blob_hash,
                size: file.size,
                normalized_mode: file.normalized_mode,
            },
        );
    }
    Ok(CompletionMarker {
        kind: COMPLETION_MARKER_KIND.to_owned(),
        schema: COMPLETION_MARKER_SCHEMA,
        snapshot_hash: snapshot_hash.to_owned(),
        tree_hash: cached_tree_hash(&directories, &files)?,
        directories,
        files,
    })
}

fn read_completion_marker(path: &Path, snapshot_hash: &str) -> Result<CompletionMarker> {
    let file = open_regular_no_follow(path)?
        .ok_or_else(|| anyhow::anyhow!("materialization completion marker is missing"))?;
    read_completion_marker_file(file, path, snapshot_hash)
}

fn read_completion_marker_file(
    file: fs::File,
    path: &Path,
    snapshot_hash: &str,
) -> Result<CompletionMarker> {
    let length = file.metadata()?.len();
    if length > MAX_COMPLETION_MARKER_BYTES {
        anyhow::bail!(
            "materialization completion marker exceeds {} bytes: {}",
            MAX_COMPLETION_MARKER_BYTES,
            path.display()
        );
    }
    let mut bytes = Vec::with_capacity(usize::try_from(length).unwrap_or(0));
    file.take(MAX_COMPLETION_MARKER_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_COMPLETION_MARKER_BYTES {
        anyhow::bail!("materialization completion marker grew while being read");
    }
    let marker: CompletionMarker = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse completion marker {}", path.display()))?;
    if marker.kind != COMPLETION_MARKER_KIND || marker.schema != COMPLETION_MARKER_SCHEMA {
        anyhow::bail!(
            "unsupported materialization completion marker at {}",
            path.display()
        );
    }
    validate_canonical_hash("completion snapshot hash", &marker.snapshot_hash)?;
    validate_canonical_hash("completion tree hash", &marker.tree_hash)?;
    if marker.snapshot_hash != snapshot_hash {
        anyhow::bail!(
            "completion marker snapshot {} does not match requested {}",
            marker.snapshot_hash,
            snapshot_hash
        );
    }
    for relative in &marker.directories {
        ryeos_state::project_sync::validate_safe_relative_path(relative)?;
    }
    for (relative, entry) in &marker.files {
        ryeos_state::project_sync::validate_safe_relative_path(relative)?;
        ryeos_state::objects::ProjectFile {
            blob_hash: entry.blob_hash.clone(),
            size: entry.size,
            normalized_mode: entry.normalized_mode,
        }
        .validate()?;
    }
    let expected_tree_hash = cached_tree_hash(&marker.directories, &marker.files)?;
    if marker.tree_hash != expected_tree_hash {
        anyhow::bail!(
            "completion marker tree hashes to {}, claims {}",
            expected_tree_hash,
            marker.tree_hash
        );
    }
    let canonical = canonical_completion_marker(&marker)?;
    if bytes != canonical.as_bytes() {
        anyhow::bail!(
            "completion marker is not canonically encoded: {}",
            path.display()
        );
    }
    Ok(marker)
}

fn open_regular_no_follow(path: &Path) -> Result<Option<fs::File>> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    match options.open(path) {
        Ok(file) => {
            if !file.metadata()?.file_type().is_file() {
                anyhow::bail!("expected a regular cache file: {}", path.display());
            }
            Ok(Some(file))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn remove_file_if_present(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn remove_path_no_follow(path: &Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_dir() {
        fs::remove_dir_all(path)?;
    } else {
        fs::remove_file(path)?;
    }
    Ok(())
}

fn staging_snapshot_hash(name: &str, marker: bool) -> Option<&str> {
    let name = if marker {
        name.strip_prefix('.')?
    } else {
        name
    };
    let (snapshot_hash, suffix) = name.split_once(".staging.")?;
    if validate_canonical_hash("staging snapshot hash", snapshot_hash).is_err() {
        return None;
    }
    let mut components = suffix.split('.');
    let pid = components.next()?;
    let random = components.next();
    if pid.is_empty()
        || !pid.bytes().all(|byte| byte.is_ascii_digit())
        || random.is_some_and(|value| {
            value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit())
        })
        || components.next().is_some()
    {
        return None;
    }
    Some(snapshot_hash)
}

#[cfg(unix)]
fn file_identity(metadata: &fs::Metadata) -> FileIdentity {
    use std::os::unix::fs::MetadataExt as _;
    FileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
        size: metadata.len(),
        mode: metadata.mode(),
        modified_seconds: metadata.mtime(),
        modified_nanoseconds: metadata.mtime_nsec(),
        changed_seconds: metadata.ctime(),
        changed_nanoseconds: metadata.ctime_nsec(),
    }
}

fn digest_verified_identity(file: &mut fs::File) -> Result<(String, fs::Metadata)> {
    #[cfg(unix)]
    let before_metadata = file.metadata()?;
    #[cfg(unix)]
    let before = file_identity(&before_metadata);
    #[cfg(unix)]
    if let Some(digest) = VERIFIED_FILE_IDENTITIES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .map_err(|_| anyhow::anyhow!("verified cache identity lock is poisoned"))?
        .get(&before)
        .cloned()
    {
        return Ok((digest, before_metadata));
    }

    file.seek(std::io::SeekFrom::Start(0))?;
    let mut digest = sha2::Sha256::new();
    use sha2::Digest as _;
    let mut buffer = [0_u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    let digest = format!("{:x}", digest.finalize());

    #[cfg(unix)]
    {
        let after_metadata = file.metadata()?;
        let after = file_identity(&after_metadata);
        if before != after {
            anyhow::bail!("cache file changed while its content was being verified");
        }
        VERIFIED_FILE_IDENTITIES
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .map_err(|_| anyhow::anyhow!("verified cache identity lock is poisoned"))?
            .insert(after, digest.clone());
        Ok((digest, after_metadata))
    }
    #[cfg(not(unix))]
    Ok((digest, file.metadata()?))
}

fn remember_verified_descriptor(
    file: &fs::File,
    expected: &ryeos_state::objects::ProjectFile,
) -> Result<()> {
    let metadata = file.metadata()?;
    verify_cached_file_metadata(
        Path::new("<verified-content-descriptor>"),
        &metadata,
        expected,
    )?;
    #[cfg(unix)]
    VERIFIED_FILE_IDENTITIES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .map_err(|_| anyhow::anyhow!("verified cache identity lock is poisoned"))?
        .insert(file_identity(&metadata), expected.blob_hash.clone());
    #[cfg(not(unix))]
    {
        let mut file = file;
        let (actual, _metadata) = digest_verified_identity(&mut file)?;
        if actual != expected.blob_hash {
            anyhow::bail!(
                "immutable content cache entry hashes to {}, expected {}: {}",
                actual,
                expected.blob_hash,
                "<verified-content-descriptor>"
            );
        }
    }
    Ok(())
}

fn verify_same_file(first: &fs::File, second: &fs::File) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let first = first.metadata()?;
        let second = second.metadata()?;
        if first.dev() != second.dev() || first.ino() != second.ino() {
            anyhow::bail!("content-cache publication did not retain the verified staging inode");
        }
    }
    #[cfg(not(unix))]
    let _ = (first, second);
    Ok(())
}

fn verify_opened_cached_file(
    mut file: fs::File,
    path: &Path,
    expected: &ryeos_state::objects::ProjectFile,
) -> Result<fs::File> {
    verify_cached_file_metadata(path, &file.metadata()?, expected)?;
    let (actual, metadata) = digest_verified_identity(&mut file)?;
    // Recheck the stable identity returned by hashing so a concurrent metadata
    // change cannot pass only the cheap preflight.
    verify_cached_file_metadata(path, &metadata, expected)?;
    if actual != expected.blob_hash {
        anyhow::bail!(
            "immutable content cache entry hashes to {}, expected {}: {}",
            actual,
            expected.blob_hash,
            path.display()
        );
    }
    Ok(file)
}

fn verify_cached_file_metadata(
    path: &Path,
    metadata: &fs::Metadata,
    expected: &ryeos_state::objects::ProjectFile,
) -> Result<()> {
    if !metadata.file_type().is_file() || metadata.len() != expected.size {
        anyhow::bail!(
            "immutable content cache entry has wrong type or size: {}",
            path.display()
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if metadata.permissions().mode() & 0o777 != expected.normalized_mode {
            anyhow::bail!(
                "immutable content cache entry has wrong mode: {}",
                path.display()
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot_hash() -> String {
        "ab".repeat(32)
    }

    fn write_regular(path: &Path, bytes: &[u8]) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, bytes).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(path, fs::Permissions::from_mode(0o644)).unwrap();
        }
    }

    #[test]
    fn completion_marker_binds_exact_tree_membership_and_content() {
        let root = tempfile::tempdir().unwrap();
        let cache_root = root.path().join("cache");
        fs::create_dir_all(&cache_root).unwrap();
        let hash = snapshot_hash();
        let staging = cache_root.join(format!("{hash}.staging.1.1"));
        write_regular(&staging.join("data/input.txt"), b"first");

        let cache = MaterializationCache::new(cache_root.clone());
        let pinned_root = cache.pinned_root().unwrap();
        let staging_name = staging.file_name().unwrap();
        let pinned_staging = pinned_root
            .open_child_directory(staging_name)
            .unwrap()
            .unwrap();
        cache
            .publish_tree(&pinned_root, staging_name, &pinned_staging, &hash)
            .unwrap();
        assert!(cache.is_complete(&hash));

        let marker_path = cache_root.join(".complete").join(&hash);
        let marker_bytes = fs::read(&marker_path).unwrap();
        let marker: CompletionMarker = serde_json::from_slice(&marker_bytes).unwrap();
        assert_eq!(marker.snapshot_hash, hash);
        assert_eq!(marker.directories, BTreeSet::from(["data".to_owned()]));
        assert_eq!(marker.files.len(), 1);
        assert_eq!(
            marker_bytes,
            canonical_completion_marker(&marker).unwrap().as_bytes()
        );

        let final_dir = cache.cache_dir(&hash);
        write_regular(&final_dir.join("extra.txt"), b"extra");
        assert!(!cache.is_complete(&hash));
        fs::remove_file(final_dir.join("extra.txt")).unwrap();
        assert!(cache.is_complete(&hash));

        fs::create_dir(final_dir.join("empty")).unwrap();
        assert!(!cache.is_complete(&hash));
        fs::remove_dir(final_dir.join("empty")).unwrap();
        assert!(cache.is_complete(&hash));

        write_regular(&final_dir.join("data/input.txt"), b"other");
        assert!(!cache.is_complete(&hash));
        write_regular(&final_dir.join("data/input.txt"), b"first");
        assert!(cache.is_complete(&hash));

        fs::remove_file(final_dir.join("data/input.txt")).unwrap();
        assert!(!cache.is_complete(&hash));
    }

    #[test]
    fn incomplete_tree_cleanup_removes_tree_and_marker_together() {
        let root = tempfile::tempdir().unwrap();
        let cache_root = root.path().join("cache");
        fs::create_dir_all(&cache_root).unwrap();
        let hash = snapshot_hash();
        let staging = cache_root.join(format!("{hash}.staging.1.2"));
        write_regular(&staging.join("input.txt"), b"first");
        let cache = MaterializationCache::new(cache_root.clone());
        let pinned_root = cache.pinned_root().unwrap();
        let staging_name = staging.file_name().unwrap();
        let pinned_staging = pinned_root
            .open_child_directory(staging_name)
            .unwrap()
            .unwrap();
        cache
            .publish_tree(&pinned_root, staging_name, &pinned_staging, &hash)
            .unwrap();

        write_regular(&cache.cache_dir(&hash).join("extra.txt"), b"extra");
        cache.remove_incomplete_tree(&hash).unwrap();

        assert!(fs::symlink_metadata(cache.cache_dir(&hash)).is_err());
        assert!(fs::symlink_metadata(cache_root.join(".complete").join(hash)).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn abandoned_staging_cleanup_skips_a_live_generation_builder() {
        let root = tempfile::tempdir().unwrap();
        let cache_root = root.path().join("cache");
        fs::create_dir_all(cache_root.join(".complete")).unwrap();
        let hash = snapshot_hash();
        let tree_staging = cache_root.join(format!("{hash}.staging.7.11"));
        let marker_staging = cache_root
            .join(".complete")
            .join(format!(".{hash}.staging.7.11"));
        fs::create_dir_all(&tree_staging).unwrap();
        fs::write(&marker_staging, b"partial").unwrap();

        let cache = MaterializationCache::new(cache_root);
        let live_build = cache.generation_build_lock(&hash).unwrap();
        cache.prune_abandoned_staging().unwrap();
        assert!(tree_staging.is_dir());
        assert!(marker_staging.is_file());

        drop(live_build);
        cache.prune_abandoned_staging().unwrap();
        assert!(fs::symlink_metadata(tree_staging).is_err());
        assert!(fs::symlink_metadata(marker_staging).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn deep_prune_removes_only_inactive_generations() {
        let root = tempfile::tempdir().unwrap();
        let cache_root = root.path().join("cache");
        fs::create_dir_all(cache_root.join(".complete")).unwrap();
        let inactive_hash = "ab".repeat(32);
        let active_hash = "cd".repeat(32);
        for hash in [&inactive_hash, &active_hash] {
            write_regular(&cache_root.join(hash).join("project.txt"), b"project");
            write_regular(&cache_root.join(".complete").join(hash), b"marker");
        }

        let cache = MaterializationCache::new(cache_root.clone());
        let active_lease = cache.generation_lease(&active_hash).unwrap();
        let report = cache.prune_inactive_generations(false).unwrap();

        assert_eq!(report.inspected_generations, 2);
        assert_eq!(report.reclaimable_generations, 1);
        assert_eq!(report.active_generations, 1);
        assert!(!cache_root.join(&inactive_hash).exists());
        assert!(cache_root.join(&active_hash).is_dir());
        drop(active_lease);

        let report = cache.prune_inactive_generations(false).unwrap();
        assert_eq!(report.reclaimable_generations, 1);
        assert_eq!(report.active_generations, 0);
        assert!(!cache_root.join(&active_hash).exists());
    }

    #[test]
    fn deep_prune_dry_run_is_mutation_free() {
        let root = tempfile::tempdir().unwrap();
        let cache_root = root.path().join("cache");
        let hash = snapshot_hash();
        write_regular(&cache_root.join(&hash).join("project.txt"), b"project");
        write_regular(&cache_root.join(".complete").join(&hash), b"marker");
        let cache = MaterializationCache::new(cache_root.clone());

        let report = cache.prune_inactive_generations(true).unwrap();

        assert_eq!(report.inspected_generations, 1);
        assert_eq!(report.reclaimable_generations, 1);
        assert_eq!(report.active_generations, 0);
        assert!(report.reclaimable_files >= 2);
        assert!(cache_root.join(&hash).is_dir());
        assert!(cache_root.join(".complete").join(&hash).is_file());
        assert!(!cache_root.join(".locks").exists());
        assert!(!cache_root.join(".leases").exists());
    }
}
