//! Large-object store for large-content-tier realizations.
//!
//! Contiguous read-only files named by whole-file sha256, living under the
//! pinned state authority beside the CAS — mmap-ready, never
//! materialize-copied. The tier is semantically blind: nothing here knows
//! what a byte means. Bytes enter only through streaming ingest
//! (hash-while-write with a fixed-size chunk trail, resumable after a
//! crash), publication is atomic and immutable (0444), and eviction follows
//! the standing lane discipline: manifest-reachable roots and leased
//! objects are untouchable, everything else leaves oldest-recency first
//! when the budget says so. Verification is an ingest/scrub fact, not a
//! per-read fact — an mmap has no read-through hook, which is exactly why
//! scrub exists.
//!
//! This store is policy-blind: it does not know what a manifest is. Callers
//! hand it the reachable root set; the manifest objects that define
//! reachability live in [`crate::objects::external_large_content_manifest`].

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
#[cfg(unix)]
use std::os::fd::AsRawFd as _;
use std::path::{Path, PathBuf};

use anyhow::bail;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::objects::{LARGE_CONTENT_CHUNK_BYTES, MAX_LARGE_CONTENT_FILE_BYTES};

pub const LARGE_OBJECT_SIDECAR_SCHEMA: &str = "ryeos.large_object_sidecar.v1";

/// Default node budget for the store as a whole. Large content is the point
/// of this tier: the ceiling is generous and eviction is honest loss — a
/// re-pin re-ingests.
pub const DEFAULT_LARGE_OBJECT_STORE_BUDGET_BYTES: u64 = 512 * 1024 * 1024 * 1024;

const OBJECTS_DIR: &str = "objects";
const SIDECARS_DIR: &str = "sidecars";
const STAGING_DIR: &str = ".staging";
const LOCKS_DIR: &str = ".locks";
const LEASES_DIR: &str = ".leases";
const INGEST_IO_BUFFER_BYTES: usize = 4 * 1024 * 1024;

/// Chunk trail published beside each large object so a scrub can verify
/// streaming without any manifest in hand. The manifest carries the same
/// list under the realization's digest; the sidecar is the store-local
/// echo, written in the same publication as the object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LargeObjectSidecar {
    pub schema: String,
    pub file_sha256: String,
    pub size: u64,
    pub chunk_size: u64,
    pub chunk_hashes: Vec<String>,
}

impl LargeObjectSidecar {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != LARGE_OBJECT_SIDECAR_SCHEMA {
            bail!("large-object sidecar schema is not current");
        }
        require_store_hash(&self.file_sha256)?;
        if self.size == 0 || self.size > MAX_LARGE_CONTENT_FILE_BYTES {
            bail!("large-object sidecar size is outside the admitted bounds");
        }
        if self.chunk_size == 0 || !self.chunk_size.is_power_of_two() {
            bail!("large-object sidecar chunk size is not a positive power of two");
        }
        let expected_chunks = self.size.div_ceil(self.chunk_size);
        if self.chunk_hashes.len() as u64 != expected_chunks {
            bail!("large-object sidecar chunk count contradicts its size");
        }
        for hash in &self.chunk_hashes {
            require_store_hash(hash)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngestedLargeObject {
    pub file_sha256: String,
    pub size: u64,
    pub chunk_size: u64,
    pub chunk_hashes: Vec<String>,
    /// The object already existed; the staged copy was discarded.
    pub deduplicated: bool,
    /// Bytes accepted from a previous interrupted ingest of the same source.
    pub resumed_bytes: u64,
}

/// Identity selected by the descriptor-owning traversal before ingest.
///
/// The store receives the already-open regular file as the read authority and
/// compares these coordinates with that descriptor before reading. It then
/// compares the complete descriptor metadata again after the stream. A path
/// is deliberately absent: state mechanics must never reopen ambient source
/// names or infer node capture policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PinnedLargeObjectSourceIdentity {
    pub containing_device: u64,
    pub inode: u64,
    pub size: u64,
}

pub struct LeasedLargeObject {
    path: PathBuf,
    file: fs::File,
    _lease: fs::File,
}

impl LeasedLargeObject {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn file(&self) -> &fs::File {
        &self.file
    }

    pub fn into_parts(self) -> (PathBuf, fs::File, fs::File) {
        (self.path, self.file, self._lease)
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct LargeObjectSweepReport {
    pub inspected_objects: usize,
    pub total_bytes_before: u64,
    pub total_bytes_after: u64,
    pub evicted: Vec<(String, u64)>,
    pub retained_roots: usize,
    pub retained_leased: usize,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct LargeObjectStagingSweepReport {
    pub files: usize,
    pub bytes: u64,
}

/// One integrity defect surfaced by scrub. Findings are evidence, not
/// logs: a scrub that returns any is reporting substrate damage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "finding", rename_all = "snake_case")]
pub enum LargeObjectIntegrityFinding {
    MissingSidecar {
        file_sha256: String,
    },
    MalformedSidecar {
        file_sha256: String,
        error: String,
    },
    SizeMismatch {
        file_sha256: String,
        sidecar: u64,
        actual: u64,
    },
    ChunkMismatch {
        file_sha256: String,
        chunk_index: u64,
    },
    FileHashMismatch {
        file_sha256: String,
        computed: String,
    },
    UnreadableObject {
        file_sha256: String,
        error: String,
    },
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct LargeObjectScrubReport {
    pub objects_verified: usize,
    pub bytes_verified: u64,
    pub findings: Vec<LargeObjectIntegrityFinding>,
}

pub struct LargeObjectStore {
    objects: lillux::PinnedDirectory,
    sidecars: lillux::PinnedDirectory,
    staging: lillux::PinnedDirectory,
    locks: lillux::PinnedDirectory,
    leases: lillux::PinnedDirectory,
    chunk_bytes: u64,
}

impl LargeObjectStore {
    /// Open an existing store without creating any directory. This is the
    /// read-only entry point used by maintenance dry-runs.
    pub fn open_under(runtime_directory: &lillux::PinnedDirectory) -> anyhow::Result<Option<Self>> {
        let Some(root) = runtime_directory.open_child_directory(OsStr::new("large-objects"))?
        else {
            return Ok(None);
        };
        let open = |name: &str| -> anyhow::Result<lillux::PinnedDirectory> {
            root.open_child_directory(OsStr::new(name))?
                .ok_or_else(|| anyhow::anyhow!("large-object store is missing {name}"))
        };
        Ok(Some(Self {
            objects: open(OBJECTS_DIR)?,
            sidecars: open(SIDECARS_DIR)?,
            staging: open(STAGING_DIR)?,
            locks: open(LOCKS_DIR)?,
            leases: open(LEASES_DIR)?,
            chunk_bytes: LARGE_CONTENT_CHUNK_BYTES,
        }))
    }

    /// Open (creating on first use) the store under an already-pinned state
    /// runtime directory — the same trust root the CAS hangs from.
    pub fn open_or_create_under(
        runtime_directory: &lillux::PinnedDirectory,
    ) -> anyhow::Result<Self> {
        Self::open_with_chunk_bytes(runtime_directory, LARGE_CONTENT_CHUNK_BYTES)
    }

    /// Test seam: the chunk size shapes hashing granularity only, never
    /// object identity, so exercising the machinery with small chunks is
    /// faithful.
    pub fn open_with_chunk_bytes(
        runtime_directory: &lillux::PinnedDirectory,
        chunk_bytes: u64,
    ) -> anyhow::Result<Self> {
        if chunk_bytes == 0 {
            bail!("large-object store chunk size must be positive");
        }
        let root = runtime_directory.open_or_create_child(OsStr::new("large-objects"), 0o700)?;
        Ok(Self {
            objects: root.open_or_create_child(OsStr::new(OBJECTS_DIR), 0o700)?,
            sidecars: root.open_or_create_child(OsStr::new(SIDECARS_DIR), 0o700)?,
            staging: root.open_or_create_child(OsStr::new(STAGING_DIR), 0o700)?,
            locks: root.open_or_create_child(OsStr::new(LOCKS_DIR), 0o700)?,
            leases: root.open_or_create_child(OsStr::new(LEASES_DIR), 0o700)?,
            chunk_bytes,
        })
    }

    pub fn chunk_bytes(&self) -> u64 {
        self.chunk_bytes
    }

    pub fn filesystem_capacity(&self) -> anyhow::Result<lillux::FilesystemCapacity> {
        self.objects.filesystem_capacity()
    }

    pub fn total_stored_bytes(&self) -> anyhow::Result<u64> {
        let mut total = 0_u64;
        for entry in self.objects.entries_no_follow()? {
            if entry.entry_type != lillux::PinnedEntryType::Regular {
                anyhow::bail!("large-object namespace contains a non-regular entry");
            }
            let file = self
                .objects
                .open_regular(&entry.name, false)?
                .ok_or_else(|| anyhow::anyhow!("large object disappeared during accounting"))?;
            total = total
                .checked_add(file.metadata()?.len())
                .ok_or_else(|| anyhow::anyhow!("large-object store byte count overflow"))?;
        }
        Ok(total)
    }

    /// Size of a stored object, or `None` when absent. Presence checks for
    /// admission use this; it takes no lease.
    pub fn object_size(&self, file_sha256: &str) -> anyhow::Result<Option<u64>> {
        require_store_hash(file_sha256)?;
        match self.objects.open_regular(OsStr::new(file_sha256), false)? {
            Some(file) => Ok(Some(file.metadata()?.len())),
            None => Ok(None),
        }
    }

    pub fn sidecar(&self, file_sha256: &str) -> anyhow::Result<Option<LargeObjectSidecar>> {
        require_store_hash(file_sha256)?;
        let Some(mut file) = self.sidecars.open_regular(OsStr::new(file_sha256), false)? else {
            return Ok(None);
        };
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        let sidecar: LargeObjectSidecar = serde_json::from_slice(&bytes)?;
        sidecar.validate()?;
        let canonical = lillux::canonical_json(&serde_json::to_value(&sidecar)?)?;
        if canonical.as_bytes() != bytes {
            bail!("large-object sidecar is not canonically encoded");
        }
        if sidecar.file_sha256 != file_sha256 {
            bail!("large-object sidecar identity contradicts its store key");
        }
        Ok(Some(sidecar))
    }

    /// Prove that an immutable object and its canonical sidecar are resident
    /// at the expected content identity and size.
    pub fn verify_resident_object(
        &self,
        file_sha256: &str,
        expected_size: u64,
    ) -> anyhow::Result<LargeObjectSidecar> {
        let size = self
            .object_size(file_sha256)?
            .ok_or_else(|| anyhow::anyhow!("large object {file_sha256} is absent"))?;
        if size != expected_size {
            bail!("large object {file_sha256} is {size} bytes; expected {expected_size}");
        }
        let sidecar = self
            .sidecar(file_sha256)?
            .ok_or_else(|| anyhow::anyhow!("large object {file_sha256} has no sidecar"))?;
        if sidecar.size != expected_size {
            bail!("large-object sidecar size contradicts retained bytes");
        }
        Ok(sidecar)
    }

    pub fn verify_manifest_commitment(
        &self,
        entry: &crate::objects::ExternalLargeContentManifestEntry,
    ) -> anyhow::Result<()> {
        let file_sha256 = entry.file_sha256.as_deref().ok_or_else(|| {
            anyhow::anyhow!("large-object verification requires a large file entry")
        })?;
        let size = entry
            .size
            .ok_or_else(|| anyhow::anyhow!("large-object manifest entry has no size"))?;
        let chunk_size = entry
            .chunk_size
            .ok_or_else(|| anyhow::anyhow!("large-object manifest entry has no chunk size"))?;
        let sidecar = self.verify_resident_object(file_sha256, size)?;
        if sidecar.chunk_size != chunk_size || sidecar.chunk_hashes != entry.chunk_hashes {
            bail!(
                "large object {} sidecar contradicts its admitted manifest",
                file_sha256
            );
        }
        Ok(())
    }

    /// Stream one source file into the store. Identity is born here: the
    /// returned hash is computed from the bytes actually staged. Resumable —
    /// an interrupted ingest of the same source picks up where it stopped,
    /// after byte-verifying the staged prefix against the source so a
    /// changed source can never publish a spliced file.
    pub fn ingest_open_regular(
        &self,
        mut source: fs::File,
        expected_source: PinnedLargeObjectSourceIdentity,
        source_label: &str,
        expected_sha256: Option<&str>,
    ) -> anyhow::Result<IngestedLargeObject> {
        if let Some(expected) = expected_sha256 {
            require_store_hash(expected)?;
        }
        if source_label.is_empty() || source_label.len() > 4_096 {
            bail!("large-content source label is empty or unbounded");
        }
        #[cfg(not(unix))]
        {
            let _ = (&mut source, expected_source, source_label);
            bail!("descriptor-pinned large-object ingest is unavailable on this platform");
        }
        #[cfg(unix)]
        let metadata = {
            use std::os::unix::fs::MetadataExt as _;
            let metadata = source.metadata()?;
            if !metadata.file_type().is_file() {
                bail!("large-content source {source_label} is not a regular file");
            }
            if metadata.dev() != expected_source.containing_device
                || metadata.ino() != expected_source.inode
                || metadata.len() != expected_source.size
            {
                bail!(
                    "large-content source {source_label} descriptor does not match its admitted inode identity"
                );
            }
            metadata
        };
        let source_len = metadata.len();
        if source_len == 0 {
            bail!(
                "large-content source {source_label} is empty; large content is never an empty file"
            );
        }
        if source_len > MAX_LARGE_CONTENT_FILE_BYTES {
            bail!(
                "large-content source {source_label} is {source_len} bytes; the per-file bound is {MAX_LARGE_CONTENT_FILE_BYTES}"
            );
        }

        // The opaque staging identity contains no source pathname. Matching
        // descriptor coordinates make an interrupted read resumable; the
        // byte-for-byte prefix comparison below remains the content proof.
        let staging_key = staging_key_for_source(expected_source);
        let staging_name = OsString::from(format!("ingest-{staging_key}"));
        let lock = self.locks.open_regular_create(
            OsStr::new(&format!("ingest-{staging_key}")),
            true,
            false,
            0o600,
        )?;
        flock_exclusive_blocking(&lock)?;

        let mut staged = self
            .staging
            .open_regular_create(&staging_name, true, false, 0o600)?;
        let mut staged_len = staged.metadata()?.len();
        if staged_len > source_len {
            staged.set_len(0)?;
            staged_len = 0;
        }

        let mut hasher = ChunkedHasher::new(self.chunk_bytes);
        let mut resumed_bytes = 0u64;
        if staged_len > 0 {
            match verify_staged_prefix(&mut staged, &mut source, staged_len, &mut hasher)? {
                true => resumed_bytes = staged_len,
                false => {
                    staged.set_len(0)?;
                    staged_len = 0;
                    hasher = ChunkedHasher::new(self.chunk_bytes);
                    source.seek(SeekFrom::Start(0))?;
                }
            }
        }

        staged.seek(SeekFrom::Start(staged_len))?;
        source.seek(SeekFrom::Start(staged_len))?;
        let mut buffer = vec![0u8; INGEST_IO_BUFFER_BYTES];
        let mut copied = staged_len;
        loop {
            let read = source.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            copied = copied
                .checked_add(read as u64)
                .ok_or_else(|| anyhow::anyhow!("large-object ingest byte count overflow"))?;
            if copied > source_len {
                bail!(
                    "large-content source {} grew during ingest; refusing to publish moving bytes",
                    source_label
                );
            }
            staged.write_all(&buffer[..read])?;
            hasher.update(&buffer[..read]);
        }
        if copied != source_len {
            bail!(
                "large-content source {} shrank during ingest: staged {copied} of {source_len} bytes",
                source_label
            );
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            let after = source.metadata()?;
            let unchanged = metadata.dev() == after.dev()
                && metadata.ino() == after.ino()
                && metadata.size() == after.size()
                && metadata.mtime() == after.mtime()
                && metadata.mtime_nsec() == after.mtime_nsec()
                && metadata.ctime() == after.ctime()
                && metadata.ctime_nsec() == after.ctime_nsec()
                && metadata.mode() == after.mode()
                && copied == after.size();
            if !unchanged {
                bail!("large-content source {source_label} changed during ingest");
            }
        }
        staged.sync_all()?;
        let (file_sha256, chunk_hashes) = hasher.finish();
        if let Some(expected) = expected_sha256
            && expected != file_sha256
        {
            bail!(
                "large-content source {} hashed to {file_sha256}, expected {expected}; \
                 staging retained for inspection",
                source_label
            );
        }

        let published = self.publish_staged(&staging_name, &staged, &file_sha256, source_len)?;
        let sidecar = LargeObjectSidecar {
            schema: LARGE_OBJECT_SIDECAR_SCHEMA.to_string(),
            file_sha256: file_sha256.clone(),
            size: source_len,
            chunk_size: self.chunk_bytes,
            chunk_hashes: chunk_hashes.clone(),
        };
        self.publish_sidecar(&sidecar)?;
        self.touch_recency(&file_sha256)?;
        drop(lock);
        Ok(IngestedLargeObject {
            file_sha256,
            size: source_len,
            chunk_size: self.chunk_bytes,
            chunk_hashes,
            deduplicated: !published,
            resumed_bytes,
        })
    }

    /// Move the staged file into the immutable namespace. Returns false when
    /// an identical object was already published (dedup — the staged copy is
    /// discarded).
    fn publish_staged(
        &self,
        staging_name: &OsStr,
        staged: &fs::File,
        file_sha256: &str,
        size: u64,
    ) -> anyhow::Result<bool> {
        if let Some(existing) = self.objects.open_regular(OsStr::new(file_sha256), false)? {
            let existing_len = existing.metadata()?.len();
            if existing_len != size {
                bail!(
                    "stored large object {file_sha256} is {existing_len} bytes but the same \
                     hash just ingested at {size}; store integrity is broken"
                );
            }
            self.staging.remove_if_same(staging_name, staged)?;
            return Ok(false);
        }
        lillux::secure_fs::set_open_regular_file_mode(staged, 0o444)?;
        match self.objects.rename_regular_from_atomic(
            OsStr::new(file_sha256),
            &self.staging,
            staging_name,
            staged,
        ) {
            Ok(()) => {}
            Err(error) => {
                // A concurrent ingest of identical bytes may have published
                // first; losing that race is dedup, not failure.
                if self
                    .objects
                    .open_regular(OsStr::new(file_sha256), false)?
                    .is_some()
                {
                    let _ = self.staging.remove_if_same(staging_name, staged);
                    return Ok(false);
                }
                return Err(anyhow::Error::from(error)
                    .context(format!("publishing large object {file_sha256}")));
            }
        }
        self.objects.sync()?;
        Ok(true)
    }

    fn publish_sidecar(&self, sidecar: &LargeObjectSidecar) -> anyhow::Result<()> {
        sidecar.validate()?;
        let bytes = lillux::canonical_json(&serde_json::to_value(sidecar)?)?.into_bytes();
        if self
            .sidecars
            .atomic_create_regular(OsStr::new(&sidecar.file_sha256), &bytes, 0o444)?
            .is_none()
        {
            let existing = self
                .sidecar(&sidecar.file_sha256)?
                .ok_or_else(|| anyhow::anyhow!("concurrent sidecar publication disappeared"))?;
            if existing != *sidecar {
                bail!(
                    "existing sidecar for {} diverges from the computed commitment",
                    sidecar.file_sha256
                );
            }
        }
        self.sidecars.sync()?;
        Ok(())
    }

    fn touch_recency(&self, file_sha256: &str) -> anyhow::Result<()> {
        let lease = self
            .leases
            .open_regular_create(OsStr::new(file_sha256), true, false, 0o600)?;
        let _ = lease.set_modified(std::time::SystemTime::now());
        Ok(())
    }

    /// Open a stored object for binding, holding a shared lease that keeps
    /// the sweep off it for as long as the returned value lives.
    pub fn lease_object(
        &self,
        file_sha256: &str,
        expected_size: u64,
    ) -> anyhow::Result<LeasedLargeObject> {
        require_store_hash(file_sha256)?;
        let name = OsStr::new(file_sha256);
        let first = self
            .objects
            .open_regular(name, false)?
            .ok_or_else(|| anyhow::anyhow!("large object {file_sha256} is not in the store"))?;
        let lease = self.leases.open_regular_create(name, true, false, 0o600)?;
        flock_shared_blocking(&lease)?;
        let _ = lease.set_modified(std::time::SystemTime::now());
        // The lease was acquired after opening; re-open and require the same
        // inode so an eviction that raced the acquisition is detected rather
        // than serving a deleted (or republished) file.
        let second = self.objects.open_regular(name, false)?.ok_or_else(|| {
            anyhow::anyhow!("large object {file_sha256} was evicted while binding")
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            let before = first.metadata()?;
            let after = second.metadata()?;
            if before.dev() != after.dev() || before.ino() != after.ino() {
                bail!("large object {file_sha256} changed identity while binding");
            }
        }
        let size = second.metadata()?.len();
        if size != expected_size {
            bail!(
                "large object {file_sha256} is {size} bytes; the manifest sealed {expected_size}"
            );
        }
        let path = self.objects.descriptor_child_path(name)?;
        Ok(LeasedLargeObject {
            path,
            file: second,
            _lease: lease,
        })
    }

    /// Hard-link a stored object into a caller-owned directory (same
    /// filesystem, zero copy). `expected` must be the leased handle for the
    /// object — the link is made from that exact inode, so a concurrent
    /// republish can never splice a different file under the same name.
    pub fn link_object_into(
        &self,
        file_sha256: &str,
        target_directory: &lillux::PinnedDirectory,
        target_name: &std::ffi::OsStr,
        expected: &fs::File,
    ) -> anyhow::Result<()> {
        require_store_hash(file_sha256)?;
        target_directory.link_regular_from(
            target_name,
            &self.objects,
            OsStr::new(file_sha256),
            expected,
        )?;
        Ok(())
    }

    /// Lease-respecting, root-respecting eviction back under the byte
    /// budget, oldest recency first. Roots are the caller's reachability
    /// answer (every large object named by a manifest an admitted capsule
    /// still references); the store never guesses at them.
    pub fn sweep_to_budget(
        &self,
        budget_bytes: u64,
        roots: &BTreeSet<String>,
    ) -> anyhow::Result<LargeObjectSweepReport> {
        self.sweep_to_budget_with_mode(budget_bytes, roots, false)
    }

    /// Plan or execute a lease-aware sweep. Dry-run acquires no new lease
    /// files and removes nothing; it reports the exact unleased candidates it
    /// would evict in the same order as a live sweep.
    pub fn sweep_to_budget_with_mode(
        &self,
        budget_bytes: u64,
        roots: &BTreeSet<String>,
        dry_run: bool,
    ) -> anyhow::Result<LargeObjectSweepReport> {
        let mut report = LargeObjectSweepReport::default();
        let mut candidates = Vec::new();
        for entry in self.objects.entries_no_follow()? {
            if entry.entry_type != lillux::PinnedEntryType::Regular {
                anyhow::bail!("large-object namespace contains a non-regular entry");
            }
            let name = entry
                .name
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("large-object namespace contains a non-UTF8 name"))?
                .to_owned();
            require_store_hash(&name)?;
            let Some(file) = self.objects.open_regular(OsStr::new(&name), false)? else {
                continue;
            };
            report.inspected_objects = report.inspected_objects.saturating_add(1);
            let bytes = file.metadata()?.len();
            report.total_bytes_before = report.total_bytes_before.saturating_add(bytes);
            if roots.contains(&name) {
                report.retained_roots += 1;
                continue;
            }
            let recency = self
                .leases
                .open_regular(OsStr::new(&name), false)?
                .and_then(|lease| lease.metadata().ok())
                .and_then(|metadata| metadata.modified().ok());
            candidates.push((name, bytes, recency));
        }
        report.total_bytes_after = report.total_bytes_before;
        if report.total_bytes_before <= budget_bytes {
            return Ok(report);
        }
        candidates.sort_by_key(|(_, _, recency)| *recency);
        for (name, bytes, _) in candidates {
            if report.total_bytes_after <= budget_bytes {
                break;
            }
            let lease = match self.leases.open_regular(OsStr::new(&name), false)? {
                Some(lease) => lease,
                None if dry_run => {
                    report.total_bytes_after = report.total_bytes_after.saturating_sub(bytes);
                    report.evicted.push((name, bytes));
                    continue;
                }
                None => self
                    .leases
                    .open_regular_create(OsStr::new(&name), true, false, 0o600)?,
            };
            if !flock_exclusive_nonblocking(&lease)? {
                report.retained_leased += 1;
                continue;
            }
            if dry_run {
                report.total_bytes_after = report.total_bytes_after.saturating_sub(bytes);
                report.evicted.push((name, bytes));
                continue;
            }
            let Some(object) = self.objects.open_regular(OsStr::new(&name), false)? else {
                continue;
            };
            self.objects.remove_if_same(OsStr::new(&name), &object)?;
            if let Some(sidecar) = self.sidecars.open_regular(OsStr::new(&name), false)? {
                let _ = self.sidecars.remove_if_same(OsStr::new(&name), &sidecar);
            }
            let _ = self.leases.remove_if_same(OsStr::new(&name), &lease);
            report.total_bytes_after = report.total_bytes_after.saturating_sub(bytes);
            report.evicted.push((name, bytes));
        }
        Ok(report)
    }

    /// Whether an execution currently holds a shared lease on this object.
    /// Absence of a lease file means unleased; inspection never creates store
    /// state.
    pub fn object_is_leased(&self, file_sha256: &str) -> anyhow::Result<bool> {
        require_store_hash(file_sha256)?;
        let Some(lease) = self.leases.open_regular(OsStr::new(file_sha256), false)? else {
            return Ok(false);
        };
        Ok(!flock_exclusive_nonblocking(&lease)?)
    }

    /// Remove staging leftovers from ingests that died. Safe at any time:
    /// live ingests hold their staging lock exclusively.
    pub fn sweep_abandoned_staging(&self) -> anyhow::Result<usize> {
        Ok(self.sweep_abandoned_staging_with_mode(false)?.files)
    }

    /// Inspect or remove abandoned ingest files. A live ingest keeps its
    /// matching lock held exclusively, so neither mode mistakes it for
    /// reclaimable state.
    pub fn sweep_abandoned_staging_with_mode(
        &self,
        dry_run: bool,
    ) -> anyhow::Result<LargeObjectStagingSweepReport> {
        let mut report = LargeObjectStagingSweepReport::default();
        for entry in self.staging.entries_no_follow()? {
            if entry.entry_type != lillux::PinnedEntryType::Regular {
                anyhow::bail!("large-object staging namespace contains a non-regular entry");
            }
            let lock = match self.locks.open_regular(&entry.name, false)? {
                Some(lock) => lock,
                None if dry_run => {
                    if let Some(staged) = self.staging.open_regular(&entry.name, false)? {
                        report.files = report.files.saturating_add(1);
                        report.bytes = report.bytes.saturating_add(staged.metadata()?.len());
                    }
                    continue;
                }
                None => self
                    .locks
                    .open_regular_create(&entry.name, true, false, 0o600)?,
            };
            if !flock_exclusive_nonblocking(&lock)? {
                continue;
            }
            if let Some(staged) = self.staging.open_regular(&entry.name, false)? {
                let bytes = staged.metadata()?.len();
                if !dry_run {
                    self.staging.remove_if_same(&entry.name, &staged)?;
                }
                report.files = report.files.saturating_add(1);
                report.bytes = report.bytes.saturating_add(bytes);
            }
            drop(lock);
        }
        Ok(report)
    }

    /// Re-derive every stored object's chunk trail and whole-file hash,
    /// streaming, one chunk in memory. Holds each object's shared lease for
    /// the duration of its scrub so eviction cannot yank bytes mid-read.
    pub fn scrub_all(&self) -> anyhow::Result<LargeObjectScrubReport> {
        let mut report = LargeObjectScrubReport::default();
        for entry in self.objects.entries_no_follow()? {
            if entry.entry_type != lillux::PinnedEntryType::Regular {
                continue;
            }
            let Some(name) = entry.name.to_str().map(str::to_owned) else {
                continue;
            };
            let findings = self.scrub_object(&name)?;
            if findings.is_empty() {
                report.objects_verified += 1;
                if let Ok(Some(size)) = self.object_size(&name) {
                    report.bytes_verified = report.bytes_verified.saturating_add(size);
                }
            } else {
                report.findings.extend(findings);
            }
        }
        Ok(report)
    }

    pub fn scrub_object(
        &self,
        file_sha256: &str,
    ) -> anyhow::Result<Vec<LargeObjectIntegrityFinding>> {
        require_store_hash(file_sha256)?;
        let mut findings = Vec::new();
        let lease = self
            .leases
            .open_regular_create(OsStr::new(file_sha256), true, false, 0o600)?;
        flock_shared_blocking(&lease)?;
        let Some(mut object) = self.objects.open_regular(OsStr::new(file_sha256), false)? else {
            return Ok(vec![LargeObjectIntegrityFinding::UnreadableObject {
                file_sha256: file_sha256.to_string(),
                error: "absent while leased for scrub".to_string(),
            }]);
        };
        let sidecar = match self.sidecar(file_sha256) {
            Ok(Some(sidecar)) => Some(sidecar),
            Ok(None) => {
                findings.push(LargeObjectIntegrityFinding::MissingSidecar {
                    file_sha256: file_sha256.to_string(),
                });
                None
            }
            Err(error) => {
                findings.push(LargeObjectIntegrityFinding::MalformedSidecar {
                    file_sha256: file_sha256.to_string(),
                    error: error.to_string(),
                });
                None
            }
        };
        let actual = object.metadata()?.len();
        if let Some(sidecar) = &sidecar
            && sidecar.size != actual
        {
            findings.push(LargeObjectIntegrityFinding::SizeMismatch {
                file_sha256: file_sha256.to_string(),
                sidecar: sidecar.size,
                actual,
            });
        }

        let chunk_size = sidecar
            .as_ref()
            .map(|sidecar| sidecar.chunk_size)
            .filter(|size| *size > 0)
            .unwrap_or(self.chunk_bytes);
        let mut hasher = ChunkedHasher::new(chunk_size);
        let mut buffer = vec![0u8; INGEST_IO_BUFFER_BYTES.min(chunk_size.max(4096) as usize)];
        object.seek(SeekFrom::Start(0))?;
        loop {
            let read = match object.read(&mut buffer) {
                Ok(read) => read,
                Err(error) => {
                    findings.push(LargeObjectIntegrityFinding::UnreadableObject {
                        file_sha256: file_sha256.to_string(),
                        error: error.to_string(),
                    });
                    return Ok(findings);
                }
            };
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        let (computed, chunk_hashes) = hasher.finish();
        if computed != file_sha256 {
            findings.push(LargeObjectIntegrityFinding::FileHashMismatch {
                file_sha256: file_sha256.to_string(),
                computed,
            });
        }
        if let Some(sidecar) = &sidecar {
            for (index, (stored, derived)) in sidecar
                .chunk_hashes
                .iter()
                .zip(chunk_hashes.iter())
                .enumerate()
            {
                if stored != derived {
                    findings.push(LargeObjectIntegrityFinding::ChunkMismatch {
                        file_sha256: file_sha256.to_string(),
                        chunk_index: index as u64,
                    });
                }
            }
            if sidecar.chunk_hashes.len() != chunk_hashes.len() {
                findings.push(LargeObjectIntegrityFinding::SizeMismatch {
                    file_sha256: file_sha256.to_string(),
                    sidecar: sidecar.size,
                    actual,
                });
            }
        }
        Ok(findings)
    }
}

fn staging_key_for_source(source: PinnedLargeObjectSourceIdentity) -> String {
    lillux::cas::sha256_hex(
        format!(
            "{}\n{}\n{}",
            source.containing_device, source.inode, source.size
        )
        .as_bytes(),
    )
}

/// Streaming whole-file + fixed-chunk hashing over one pass of bytes.
struct ChunkedHasher {
    chunk_bytes: u64,
    whole: Sha256,
    chunk: Sha256,
    chunk_filled: u64,
    chunk_hashes: Vec<String>,
    any_bytes: bool,
}

impl ChunkedHasher {
    fn new(chunk_bytes: u64) -> Self {
        Self {
            chunk_bytes,
            whole: Sha256::new(),
            chunk: Sha256::new(),
            chunk_filled: 0,
            chunk_hashes: Vec::new(),
            any_bytes: false,
        }
    }

    fn update(&mut self, mut bytes: &[u8]) {
        self.any_bytes |= !bytes.is_empty();
        self.whole.update(bytes);
        while !bytes.is_empty() {
            let remaining = (self.chunk_bytes - self.chunk_filled) as usize;
            let take = remaining.min(bytes.len());
            self.chunk.update(&bytes[..take]);
            self.chunk_filled += take as u64;
            bytes = &bytes[take..];
            if self.chunk_filled == self.chunk_bytes {
                let finished = std::mem::replace(&mut self.chunk, Sha256::new());
                self.chunk_hashes.push(format!("{:x}", finished.finalize()));
                self.chunk_filled = 0;
            }
        }
    }

    fn finish(mut self) -> (String, Vec<String>) {
        if self.chunk_filled > 0 || (self.any_bytes && self.chunk_hashes.is_empty()) {
            self.chunk_hashes
                .push(format!("{:x}", self.chunk.finalize()));
        }
        (format!("{:x}", self.whole.finalize()), self.chunk_hashes)
    }
}

/// Byte-verify the staged prefix against the source while feeding the
/// hashers, one buffer at a time. Returns false on any divergence.
fn verify_staged_prefix(
    staged: &mut fs::File,
    source: &mut fs::File,
    prefix_len: u64,
    hasher: &mut ChunkedHasher,
) -> anyhow::Result<bool> {
    staged.seek(SeekFrom::Start(0))?;
    source.seek(SeekFrom::Start(0))?;
    let mut staged_buffer = vec![0u8; INGEST_IO_BUFFER_BYTES];
    let mut source_buffer = vec![0u8; INGEST_IO_BUFFER_BYTES];
    let mut remaining = prefix_len;
    while remaining > 0 {
        let want = (remaining.min(INGEST_IO_BUFFER_BYTES as u64)) as usize;
        read_exact_len(staged, &mut staged_buffer[..want])?;
        read_exact_len(source, &mut source_buffer[..want])?;
        if staged_buffer[..want] != source_buffer[..want] {
            return Ok(false);
        }
        hasher.update(&staged_buffer[..want]);
        remaining -= want as u64;
    }
    Ok(true)
}

fn read_exact_len(file: &mut fs::File, buffer: &mut [u8]) -> anyhow::Result<()> {
    file.read_exact(buffer)
        .map_err(|error| anyhow::anyhow!("short read while verifying staged ingest: {error}"))
}

fn require_store_hash(value: &str) -> anyhow::Result<()> {
    if !lillux::cas::valid_hash(value) {
        bail!("large-object store names are 64-hex sha256 digests; got {value:?}");
    }
    Ok(())
}

#[cfg(unix)]
fn flock_exclusive_blocking(file: &fs::File) -> anyhow::Result<()> {
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(unix)]
fn flock_shared_blocking(file: &fs::File) -> anyhow::Result<()> {
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

/// True when the exclusive lock was won without blocking.
#[cfg(unix)]
fn flock_exclusive_nonblocking(file: &fs::File) -> anyhow::Result<bool> {
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::WouldBlock {
            return Ok(false);
        }
        return Err(error.into());
    }
    Ok(true)
}

#[cfg(not(unix))]
fn flock_exclusive_blocking(_file: &fs::File) -> anyhow::Result<()> {
    bail!("large-object store locking is unavailable on this platform");
}

#[cfg(not(unix))]
fn flock_shared_blocking(_file: &fs::File) -> anyhow::Result<()> {
    bail!("large-object store locking is unavailable on this platform");
}

#[cfg(not(unix))]
fn flock_exclusive_nonblocking(_file: &fs::File) -> anyhow::Result<bool> {
    bail!("large-object store locking is unavailable on this platform");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(chunk: u64) -> (tempfile::TempDir, LargeObjectStore) {
        let dir = tempfile::tempdir().unwrap();
        let runtime = lillux::PinnedDirectory::open_or_create(dir.path()).unwrap();
        let store = LargeObjectStore::open_with_chunk_bytes(&runtime, chunk).unwrap();
        (dir, store)
    }

    fn write_source(dir: &tempfile::TempDir, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.path().join(name);
        fs::write(&path, bytes).unwrap();
        path
    }

    fn ingest(
        store: &LargeObjectStore,
        source: &Path,
        expected_sha256: Option<&str>,
    ) -> anyhow::Result<IngestedLargeObject> {
        use std::os::unix::fs::MetadataExt as _;
        let file = fs::File::open(source)?;
        let metadata = file.metadata()?;
        store.ingest_open_regular(
            file,
            PinnedLargeObjectSourceIdentity {
                containing_device: metadata.dev(),
                inode: metadata.ino(),
                size: metadata.len(),
            },
            source
                .file_name()
                .and_then(OsStr::to_str)
                .unwrap_or("test-source"),
            expected_sha256,
        )
    }

    #[test]
    fn ingest_publishes_an_immutable_object_with_a_faithful_chunk_trail() {
        let (dir, store) = store(8);
        let bytes = b"0123456789abcdefXYZ".to_vec();
        let source = write_source(&dir, "payload.bin", &bytes);

        let ingested = ingest(&store, &source, None).unwrap();
        assert_eq!(ingested.size, 19);
        assert_eq!(ingested.file_sha256, lillux::cas::sha256_hex(&bytes));
        assert_eq!(
            ingested.chunk_hashes,
            vec![
                lillux::cas::sha256_hex(b"01234567"),
                lillux::cas::sha256_hex(b"89abcdef"),
                lillux::cas::sha256_hex(b"XYZ"),
            ]
        );
        assert!(!ingested.deduplicated);
        assert_eq!(store.object_size(&ingested.file_sha256).unwrap(), Some(19));
        let sidecar = store.sidecar(&ingested.file_sha256).unwrap().unwrap();
        assert_eq!(sidecar.chunk_hashes, ingested.chunk_hashes);
        assert_eq!(sidecar.chunk_size, 8);

        // Identical bytes from another path dedup instead of re-publishing.
        let copy = write_source(&dir, "copy.bin", &bytes);
        let again = ingest(&store, &copy, None).unwrap();
        assert!(again.deduplicated);
        assert_eq!(again.file_sha256, ingested.file_sha256);
    }

    #[test]
    fn an_interrupted_ingest_resumes_and_a_changed_source_restarts() {
        let (dir, store) = store(8);
        let bytes = vec![7u8; 41];
        let source = write_source(&dir, "payload.bin", &bytes);

        // Simulate a crash: a staged prefix left behind by a dead ingest.
        use std::os::unix::fs::MetadataExt as _;
        let metadata = fs::metadata(&source).unwrap();
        let staging_key = staging_key_for_source(PinnedLargeObjectSourceIdentity {
            containing_device: metadata.dev(),
            inode: metadata.ino(),
            size: metadata.len(),
        });
        let staged = dir
            .path()
            .join("large-objects/.staging")
            .join(format!("ingest-{staging_key}"));
        fs::write(&staged, &bytes[..17]).unwrap();

        let resumed = ingest(&store, &source, None).unwrap();
        assert_eq!(resumed.resumed_bytes, 17);
        assert_eq!(resumed.file_sha256, lillux::cas::sha256_hex(&bytes));
        assert!(store.scrub_object(&resumed.file_sha256).unwrap().is_empty());

        // A staged prefix that no longer matches the source is discarded,
        // never spliced.
        let other = vec![9u8; 41];
        let source2 = write_source(&dir, "payload-2.bin", &other);
        let metadata = fs::metadata(&source2).unwrap();
        let key2 = staging_key_for_source(PinnedLargeObjectSourceIdentity {
            containing_device: metadata.dev(),
            inode: metadata.ino(),
            size: metadata.len(),
        });
        let staged2 = dir
            .path()
            .join("large-objects/.staging")
            .join(format!("ingest-{key2}"));
        fs::write(&staged2, vec![1u8; 17]).unwrap();
        let restarted = ingest(&store, &source2, None).unwrap();
        assert_eq!(restarted.resumed_bytes, 0);
        assert_eq!(restarted.file_sha256, lillux::cas::sha256_hex(&other));
    }

    #[test]
    fn an_expected_hash_mismatch_refuses_publication() {
        let (dir, store) = store(8);
        let source = write_source(&dir, "payload.bin", b"not the pinned bytes");
        let error = ingest(&store, &source, Some(&"a".repeat(64)))
            .unwrap_err()
            .to_string();
        assert!(error.contains("expected"), "got: {error}");
        assert_eq!(
            store
                .object_size(&lillux::cas::sha256_hex(b"not the pinned bytes"))
                .unwrap(),
            None
        );
    }

    #[test]
    fn leases_pin_objects_against_the_sweep_and_roots_are_untouchable() {
        let (dir, store) = store(8);
        let kept = ingest(&store, &write_source(&dir, "kept.bin", &[1u8; 32]), None).unwrap();
        let leased = ingest(&store, &write_source(&dir, "leased.bin", &[2u8; 32]), None).unwrap();
        let doomed = ingest(&store, &write_source(&dir, "doomed.bin", &[3u8; 32]), None).unwrap();

        let held = store.lease_object(&leased.file_sha256, 32).unwrap();
        assert_eq!(held.file().metadata().unwrap().len(), 32);

        let mut roots = BTreeSet::new();
        roots.insert(kept.file_sha256.clone());
        let report = store.sweep_to_budget(0, &roots).unwrap();
        assert_eq!(report.retained_roots, 1);
        assert_eq!(report.retained_leased, 1);
        assert_eq!(
            report
                .evicted
                .iter()
                .map(|(hash, _)| hash.as_str())
                .collect::<Vec<_>>(),
            vec![doomed.file_sha256.as_str()]
        );
        assert_eq!(store.object_size(&doomed.file_sha256).unwrap(), None);
        assert_eq!(store.object_size(&kept.file_sha256).unwrap(), Some(32));
        assert_eq!(store.object_size(&leased.file_sha256).unwrap(), Some(32));
        assert!(store.sidecar(&doomed.file_sha256).unwrap().is_none());

        drop(held);
        let report = store.sweep_to_budget(0, &roots).unwrap();
        assert_eq!(
            report
                .evicted
                .iter()
                .map(|(hash, _)| hash.as_str())
                .collect::<Vec<_>>(),
            vec![leased.file_sha256.as_str()]
        );
    }

    #[test]
    fn scrub_reports_corruption_and_a_missing_sidecar_honestly() {
        let (dir, store) = store(8);
        let bytes = vec![5u8; 24];
        let ingested = ingest(&store, &write_source(&dir, "payload.bin", &bytes), None).unwrap();
        assert!(
            store
                .scrub_object(&ingested.file_sha256)
                .unwrap()
                .is_empty()
        );
        let clean = store.scrub_all().unwrap();
        assert_eq!(clean.objects_verified, 1);
        assert_eq!(clean.bytes_verified, 24);
        assert!(clean.findings.is_empty());

        // Corrupt one byte through a fresh handle; published mode is 0444 so
        // the test reopens with explicit permissions.
        let object_path = dir
            .path()
            .join("large-objects/objects")
            .join(&ingested.file_sha256);
        let mut permissions = fs::metadata(&object_path).unwrap().permissions();
        use std::os::unix::fs::PermissionsExt as _;
        permissions.set_mode(0o644);
        fs::set_permissions(&object_path, permissions).unwrap();
        let mut corrupted = fs::read(&object_path).unwrap();
        corrupted[9] ^= 0xff;
        fs::write(&object_path, &corrupted).unwrap();

        let findings = store.scrub_object(&ingested.file_sha256).unwrap();
        assert!(
            findings.iter().any(|finding| matches!(
                finding,
                LargeObjectIntegrityFinding::ChunkMismatch { chunk_index: 1, .. }
            )),
            "got: {findings:?}"
        );
        assert!(
            findings.iter().any(|finding| matches!(
                finding,
                LargeObjectIntegrityFinding::FileHashMismatch { .. }
            )),
            "got: {findings:?}"
        );

        fs::remove_file(
            dir.path()
                .join("large-objects/sidecars")
                .join(&ingested.file_sha256),
        )
        .unwrap();
        let findings = store.scrub_object(&ingested.file_sha256).unwrap();
        assert!(
            findings.iter().any(|finding| matches!(
                finding,
                LargeObjectIntegrityFinding::MissingSidecar { .. }
            )),
            "got: {findings:?}"
        );
    }

    #[test]
    fn a_leased_object_hard_links_without_copying() {
        let (dir, store) = store(8);
        let ingested =
            ingest(&store, &write_source(&dir, "payload.bin", &[4u8; 24]), None).unwrap();
        let leased = store.lease_object(&ingested.file_sha256, 24).unwrap();
        let target = lillux::PinnedDirectory::open_or_create(&dir.path().join("mount")).unwrap();
        store
            .link_object_into(
                &ingested.file_sha256,
                &target,
                std::ffi::OsStr::new("segment-0"),
                leased.file(),
            )
            .unwrap();
        use std::os::unix::fs::MetadataExt as _;
        let linked = fs::metadata(dir.path().join("mount/segment-0")).unwrap();
        assert_eq!(linked.ino(), leased.file().metadata().unwrap().ino());
        assert_eq!(linked.len(), 24);
    }

    #[test]
    fn abandoned_staging_is_reclaimed() {
        let (dir, store) = store(8);
        fs::write(
            dir.path().join("large-objects/.staging/ingest-deadbeef"),
            b"leftover",
        )
        .unwrap();
        assert_eq!(store.sweep_abandoned_staging().unwrap(), 1);
        assert_eq!(store.sweep_abandoned_staging().unwrap(), 0);
    }
}
