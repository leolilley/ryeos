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
//! scrub exists. Scrub takes an object whose bytes no longer hash to its
//! name out of service by moving it into `quarantine/`: launches then
//! refuse it by name, a re-ingest of the right bytes publishes a clean copy,
//! and the damaged copy stays as evidence (counted against the budget, never
//! evicted) until an operator purges it with the node stopped.
//!
//! This store is policy-blind: it does not know what a manifest is. Callers
//! hand it the reachable root set; the manifest objects that define
//! reachability live in [`crate::objects::external_large_content_manifest`].

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};
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
const QUARANTINE_DIR: &str = "quarantine";
/// Damaged copies of one hash the quarantine keeps before scrub refuses to
/// quarantine it again; an operator purge empties it.
const MAX_QUARANTINED_COPIES: u32 = 64;
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

/// What a remote-fed upload promises about one large object, taken from the
/// manifest entry that names it. Every accepted byte is checked against it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LargeObjectExpectation {
    pub size: u64,
    pub chunk_size: u64,
    pub chunk_hashes: Vec<String>,
}

/// Where a resumable upload stands after one accepted write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StageProgress {
    /// Bytes now retained for the object (the full size once published).
    pub staged_bytes: u64,
    /// The object is published and resident with its sidecar.
    pub complete: bool,
    /// The object was already resident; nothing was written.
    pub deduplicated: bool,
}

/// Identity selected by the descriptor-owning traversal before ingest.
///
/// The store receives the already-open regular file as the read authority and
/// compares these coordinates with that descriptor before reading. It then
/// compares the complete descriptor metadata again after the stream. The file
/// identity names the filesystem's own identity, not the kernel's attachment
/// number, because the resumable staging key derived from it outlives a
/// process and a re-attached disk must still find its staged prefix. A path
/// is deliberately absent: state mechanics must never reopen ambient source
/// names or infer node capture policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PinnedLargeObjectSourceIdentity {
    pub file: lillux::PinnedRegularFileIdentity,
    pub size: u64,
}

pub struct LeasedLargeObject {
    path: PathBuf,
    file: fs::File,
    _lease: lillux::locks::ForkClosedLockFile,
}

impl LeasedLargeObject {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn file(&self) -> &fs::File {
        &self.file
    }

    /// The shared lease is the registered lock itself: it stays held exactly
    /// as long as the returned value lives.
    pub fn into_parts(self) -> (PathBuf, fs::File, lillux::locks::ForkClosedLockFile) {
        (self.path, self.file, self._lease)
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct LargeObjectSweepReport {
    pub inspected_objects: usize,
    /// Includes `quarantined_bytes`: quarantined evidence counts against the
    /// budget but is never an eviction candidate.
    pub total_bytes_before: u64,
    pub total_bytes_after: u64,
    pub evicted: Vec<(String, u64)>,
    pub retained_roots: usize,
    pub retained_leased: usize,
    pub quarantined_bytes: u64,
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
    /// An entry in the object namespace that the store did not write: not a
    /// regular file, or not named by a 64-hex digest. Scrub reports it and
    /// leaves it alone; sweep still refuses until an operator removes it.
    ForeignEntry {
        /// The entry name; bytes that are not UTF-8 are escaped.
        name: String,
        entry_type: String,
    },
}

impl LargeObjectIntegrityFinding {
    /// The object's bytes no longer hash to the name they were published
    /// under. Only this is proof of byte damage: a chunk or size mismatch
    /// beside a matching whole-file hash means the sidecar is what is wrong,
    /// and an unreadable object may be a transient I/O fault.
    pub fn is_byte_damage(&self) -> bool {
        matches!(self, Self::FileHashMismatch { .. })
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct LargeObjectScrubReport {
    pub objects_verified: usize,
    pub bytes_verified: u64,
    pub findings: Vec<LargeObjectIntegrityFinding>,
    /// Hashes whose byte-damaged object (and sidecar) this scrub moved into
    /// quarantine.
    pub quarantined: Vec<String>,
}

/// What an operator purge of the quarantine removed, or would remove.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct LargeObjectQuarantinePurgeReport {
    pub dry_run: bool,
    pub objects: usize,
    pub bytes: u64,
    pub sidecars: usize,
}

/// A launch, lease or residency check named a large object that scrub took
/// out of service. The cure is an operator act, so the refusal is typed and
/// names it rather than failing as an internal error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LargeObjectQuarantined {
    pub file_sha256: String,
}

impl std::fmt::Display for LargeObjectQuarantined {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "large object {} is quarantined: scrub found its bytes no longer match their hash. \
             Re-import the realization's original bytes (`ryeos external-content import`, or push \
             the product again) to publish a clean copy; the damaged copy is kept as evidence until \
             `ryeos node large-objects purge-quarantine --confirm` on the stopped node",
            self.file_sha256
        )
    }
}

impl std::error::Error for LargeObjectQuarantined {}

pub struct LargeObjectStore {
    root: lillux::PinnedDirectory,
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
            root,
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
            root,
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
        total
            .checked_add(self.quarantined_bytes()?)
            .ok_or_else(|| anyhow::anyhow!("large-object store byte count overflow"))
    }

    /// Bytes held as quarantined evidence. They are part of
    /// `total_stored_bytes` and of a sweep's total, but never a candidate.
    pub fn quarantined_bytes(&self) -> anyhow::Result<u64> {
        let Some(quarantine) = self.quarantine_child(OBJECTS_DIR)? else {
            return Ok(0);
        };
        let mut total = 0_u64;
        for entry in quarantine.entries_no_follow()? {
            if entry.entry_type != lillux::PinnedEntryType::Regular {
                bail!("large-object quarantine contains a non-regular entry");
            }
            let Some(file) = quarantine.open_regular(&entry.name, false)? else {
                continue;
            };
            total = total
                .checked_add(file.metadata()?.len())
                .ok_or_else(|| anyhow::anyhow!("large-object quarantine byte count overflow"))?;
        }
        Ok(total)
    }

    /// Whether scrub has quarantined a damaged copy of this hash. The clean
    /// copy a re-ingest publishes is served regardless; this only names why
    /// an absent object is absent.
    pub fn is_quarantined(&self, file_sha256: &str) -> anyhow::Result<bool> {
        require_store_hash(file_sha256)?;
        let Some(quarantine) = self.quarantine_child(OBJECTS_DIR)? else {
            return Ok(false);
        };
        Ok(quarantine
            .entry_no_follow(OsStr::new(file_sha256))?
            .is_some())
    }

    /// The refusal for an object that is not in the store: typed when scrub
    /// quarantined it, so the caller can name the cure.
    fn absent_object_error(&self, file_sha256: &str) -> anyhow::Error {
        match self.is_quarantined(file_sha256) {
            Ok(true) => LargeObjectQuarantined {
                file_sha256: file_sha256.to_owned(),
            }
            .into(),
            Ok(false) => anyhow::anyhow!("large object {file_sha256} is absent"),
            Err(error) => error.context(format!(
                "large object {file_sha256} is absent and its quarantine is unreadable"
            )),
        }
    }

    fn quarantine_child(&self, name: &str) -> anyhow::Result<Option<lillux::PinnedDirectory>> {
        let Some(quarantine) = self.root.open_child_directory(OsStr::new(QUARANTINE_DIR))? else {
            return Ok(None);
        };
        quarantine.open_child_directory(OsStr::new(name))
    }

    /// Bytes held by in-flight upload staging files. Budget admission counts
    /// these together with `total_stored_bytes`.
    pub fn staging_bytes(&self) -> anyhow::Result<u64> {
        let mut total = 0_u64;
        for entry in self.staging.entries_no_follow()? {
            if entry.entry_type != lillux::PinnedEntryType::Regular {
                anyhow::bail!("large-object staging namespace contains a non-regular entry");
            }
            if !entry.name.to_string_lossy().starts_with("upload-") {
                continue;
            }
            let Some(file) = self.staging.open_regular(&entry.name, false)? else {
                continue;
            };
            total = total
                .checked_add(file.metadata()?.len())
                .ok_or_else(|| anyhow::anyhow!("large-object staging byte count overflow"))?;
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
        let Some(size) = self.object_size(file_sha256)? else {
            return Err(self.absent_object_error(file_sha256));
        };
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
            let metadata = source.metadata()?;
            if !metadata.file_type().is_file() {
                bail!("large-content source {source_label} is not a regular file");
            }
            if lillux::pinned_regular_file_identity(&source)? != expected_source.file
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
        let staging_key = staging_key_for_source(expected_source)?;
        let staging_name = OsString::from(format!("ingest-{staging_key}"));
        let lock = open_registered_lock(|| {
            self.locks.open_regular_create(
                OsStr::new(&format!("ingest-{staging_key}")),
                true,
                false,
                0o600,
            )
        })?;
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

    /// Bytes retained toward an object: the resident size when it is already
    /// published with its sidecar, otherwise the length of its upload staging
    /// file (0 when none exists). An object that is present without its
    /// sidecar (stopped between publishing the object and its sidecar) is not
    /// resident, so the upload resends and `stage_chunk` repairs it.
    pub fn staged_bytes(&self, file_sha256: &str) -> anyhow::Result<u64> {
        require_store_hash(file_sha256)?;
        if let Some(size) = self.object_size(file_sha256)?
            && self.sidecar(file_sha256)?.is_some()
        {
            return Ok(size);
        }
        let name = format!("upload-{file_sha256}");
        match self.staging.open_regular(OsStr::new(&name), false)? {
            Some(file) => Ok(file.metadata()?.len()),
            None => Ok(0),
        }
    }

    /// Accept one write of a remote-fed, resumable upload of the object whose
    /// whole-file hash is `file_sha256`. Writes are sequential and
    /// retry-idempotent: a write that starts inside the retained range must
    /// byte-match it, and only its non-overlapping tail is appended. Each
    /// sidecar chunk is verified when it completes; the whole-file hash and
    /// the chunk trail are verified once more before the object is published
    /// through the same path as a local ingest.
    pub fn stage_chunk(
        &self,
        file_sha256: &str,
        expect: &LargeObjectExpectation,
        offset: u64,
        bytes: &[u8],
    ) -> anyhow::Result<StageProgress> {
        require_store_hash(file_sha256)?;
        self.validate_expectation(expect)?;
        let name = format!("upload-{file_sha256}");
        let lock = open_registered_lock(|| {
            self.locks
                .open_regular_create(OsStr::new(&name), true, false, 0o600)
        })?;
        flock_exclusive_blocking(&lock)?;

        if let Some(resident) = self.object_size(file_sha256)?
            && let Some(sidecar) = self.sidecar(file_sha256)?
        {
            if resident != expect.size
                || sidecar.size != expect.size
                || sidecar.chunk_size != expect.chunk_size
                || sidecar.chunk_hashes != expect.chunk_hashes
            {
                bail!(
                    "resident large object {file_sha256} contradicts the declared size and chunk trail"
                );
            }
            return Ok(StageProgress {
                staged_bytes: expect.size,
                complete: true,
                deduplicated: true,
            });
        }

        if bytes.is_empty() {
            bail!("large-object chunk at offset {offset} is empty");
        }
        let length = u64::try_from(bytes.len())?;
        let end = offset
            .checked_add(length)
            .ok_or_else(|| anyhow::anyhow!("large-object chunk range overflow"))?;
        if end > expect.size {
            bail!(
                "large-object chunk of {length} bytes at offset {offset} extends past the declared size {}",
                expect.size
            );
        }

        let staging_name = OsString::from(&name);
        // The staging file is created only once the write is admitted, so an
        // out-of-order first write leaves no empty file behind.
        let mut staged = match self.staging.open_regular(&staging_name, true)? {
            Some(file) => file,
            None => {
                if offset > 0 {
                    bail!("large-object chunk is out of order: expected offset 0, got {offset}");
                }
                self.staging
                    .open_regular_create(&staging_name, true, false, 0o600)?
            }
        };
        let mut staged_len = staged.metadata()?.len();
        if staged_len > expect.size {
            staged.set_len(0)?;
            staged_len = 0;
        }
        if offset > staged_len {
            bail!("large-object chunk is out of order: expected offset {staged_len}, got {offset}");
        }

        let original_len = staged_len;
        let tail = if offset < staged_len {
            let overlap = usize::try_from(staged_len.min(end) - offset)?;
            let mut retained = vec![0u8; overlap.min(INGEST_IO_BUFFER_BYTES)];
            let mut compared = 0usize;
            while compared < overlap {
                let take = (overlap - compared).min(retained.len());
                staged.seek(SeekFrom::Start(offset + compared as u64))?;
                read_exact_len(&mut staged, &mut retained[..take])?;
                if let Some(differs) = retained[..take]
                    .iter()
                    .zip(&bytes[compared..compared + take])
                    .position(|(staged_byte, retried_byte)| staged_byte != retried_byte)
                {
                    bail!(
                        "retried bytes differ from the staged bytes at offset {}",
                        offset + (compared + differs) as u64
                    );
                }
                compared += take;
            }
            &bytes[overlap..]
        } else {
            bytes
        };

        if !tail.is_empty() {
            staged.seek(SeekFrom::Start(staged_len))?;
            staged.write_all(tail)?;
            staged.sync_data()?;
            if original_len == 0 {
                // The first acknowledged write must include the new directory
                // entry, not only its data.
                self.staging.sync()?;
            }
            staged_len += u64::try_from(tail.len())?;
            Self::verify_completed_chunks(&mut staged, expect, original_len, staged_len)?;
        }

        if staged_len < expect.size {
            return Ok(StageProgress {
                staged_bytes: staged_len,
                complete: false,
                deduplicated: false,
            });
        }

        let mut hasher = ChunkedHasher::new(self.chunk_bytes);
        staged.seek(SeekFrom::Start(0))?;
        let mut buffer = vec![0u8; INGEST_IO_BUFFER_BYTES];
        let mut remaining = expect.size;
        while remaining > 0 {
            let want = usize::try_from(remaining.min(INGEST_IO_BUFFER_BYTES as u64))?;
            read_exact_len(&mut staged, &mut buffer[..want])?;
            hasher.update(&buffer[..want]);
            remaining -= want as u64;
        }
        let (actual_sha256, actual_chunks) = hasher.finish();
        if actual_sha256 != file_sha256 {
            self.staging.remove_if_same(&staging_name, &staged)?;
            bail!(
                "staged large object hashed to {actual_sha256}, expected {file_sha256}; staging discarded"
            );
        }
        if actual_chunks != expect.chunk_hashes {
            self.staging.remove_if_same(&staging_name, &staged)?;
            bail!(
                "staged large object {file_sha256} contradicts its declared chunk trail; staging discarded"
            );
        }

        let published = self.publish_staged(&staging_name, &staged, file_sha256, expect.size)?;
        self.publish_sidecar(&LargeObjectSidecar {
            schema: LARGE_OBJECT_SIDECAR_SCHEMA.to_string(),
            file_sha256: file_sha256.to_string(),
            size: expect.size,
            chunk_size: expect.chunk_size,
            chunk_hashes: expect.chunk_hashes.clone(),
        })?;
        self.touch_recency(file_sha256)?;
        drop(lock);
        Ok(StageProgress {
            staged_bytes: expect.size,
            complete: true,
            deduplicated: !published,
        })
    }

    fn validate_expectation(&self, expect: &LargeObjectExpectation) -> anyhow::Result<()> {
        if expect.size == 0 || expect.size > MAX_LARGE_CONTENT_FILE_BYTES {
            bail!(
                "declared large-object size {} is outside the admitted bounds",
                expect.size
            );
        }
        if expect.chunk_size != self.chunk_bytes {
            bail!(
                "declared large-object chunk size {} differs from this store's chunk size {}",
                expect.chunk_size,
                self.chunk_bytes
            );
        }
        if expect.chunk_hashes.len() as u64 != expect.size.div_ceil(expect.chunk_size) {
            bail!("declared large-object chunk count contradicts its size");
        }
        for hash in &expect.chunk_hashes {
            require_store_hash(hash)?;
        }
        Ok(())
    }

    /// Hash every sidecar chunk that an append from `from` to `to` finished
    /// and compare it with the declared trail. A failing chunk is cut off the
    /// staging file so the sender restarts at that chunk's first byte.
    fn verify_completed_chunks(
        staged: &mut fs::File,
        expect: &LargeObjectExpectation,
        from: u64,
        to: u64,
    ) -> anyhow::Result<()> {
        let mut index = from / expect.chunk_size;
        loop {
            let start = index * expect.chunk_size;
            let end = (start + expect.chunk_size).min(expect.size);
            if start >= expect.size || end > to {
                return Ok(());
            }
            if end > from {
                let actual = hash_file_range(staged, start, end - start)?;
                let declared = usize::try_from(index)
                    .ok()
                    .and_then(|position| expect.chunk_hashes.get(position));
                if declared != Some(&actual) {
                    staged.set_len(start)?;
                    staged.sync_data()?;
                    bail!("chunk {index} of the large object failed its hash");
                }
            }
            index += 1;
        }
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
            .atomic_create_regular(
                OsStr::new(&sidecar.file_sha256),
                &bytes,
                0o444,
                crate::STATE_TEMP_PURPOSE,
            )?
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
        match self.try_lease_object(file_sha256, expected_size)? {
            Some(leased) => Ok(leased),
            None => Err(self.absent_object_error(file_sha256)),
        }
    }

    /// Like `lease_object`, but an object that is absent, or evicted while the
    /// lease binds, yields `None` instead of an error.
    pub fn try_lease_object(
        &self,
        file_sha256: &str,
        expected_size: u64,
    ) -> anyhow::Result<Option<LeasedLargeObject>> {
        require_store_hash(file_sha256)?;
        let name = OsStr::new(file_sha256);
        let Some(first) = self.objects.open_regular(name, false)? else {
            return Ok(None);
        };
        let lease =
            open_registered_lock(|| self.leases.open_regular_create(name, true, false, 0o600))?;
        flock_shared_blocking(&lease)?;
        let _ = lease.set_modified(std::time::SystemTime::now());
        // The lease was acquired after opening; re-open and require the same
        // inode so an eviction that raced the acquisition is detected rather
        // than serving a deleted (or republished) file.
        let Some(second) = self.objects.open_regular(name, false)? else {
            return Ok(None);
        };
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
        Ok(Some(LeasedLargeObject {
            path,
            file: second,
            _lease: lease,
        }))
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
        report.quarantined_bytes = self.quarantined_bytes()?;
        report.total_bytes_before = report
            .total_bytes_before
            .saturating_add(report.quarantined_bytes);
        report.total_bytes_after = report.total_bytes_before;
        if report.total_bytes_before <= budget_bytes {
            return Ok(report);
        }
        candidates.sort_by_key(|(_, _, recency)| *recency);
        for (name, bytes, _) in candidates {
            if report.total_bytes_after <= budget_bytes {
                break;
            }
            let lease = match open_registered_lock_if_present(|| {
                self.leases.open_regular(OsStr::new(&name), false)
            })? {
                Some(lease) => lease,
                None if dry_run => {
                    report.total_bytes_after = report.total_bytes_after.saturating_sub(bytes);
                    report.evicted.push((name, bytes));
                    continue;
                }
                None => open_registered_lock(|| {
                    self.leases
                        .open_regular_create(OsStr::new(&name), true, false, 0o600)
                })?,
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
        let Some(lease) = open_registered_lock_if_present(|| {
            self.leases.open_regular(OsStr::new(file_sha256), false)
        })?
        else {
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
            let lock = match open_registered_lock_if_present(|| {
                self.locks.open_regular(&entry.name, false)
            })? {
                Some(lock) => lock,
                None if dry_run => {
                    if let Some(staged) = self.staging.open_regular(&entry.name, false)? {
                        report.files = report.files.saturating_add(1);
                        report.bytes = report.bytes.saturating_add(staged.metadata()?.len());
                    }
                    continue;
                }
                None => open_registered_lock(|| {
                    self.locks
                        .open_regular_create(&entry.name, true, false, 0o600)
                })?,
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
    /// the duration of its scrub so eviction cannot yank bytes mid-read. An
    /// object whose bytes no longer hash to its name is moved, with its
    /// sidecar, into quarantine while that lease is still held: holders keep
    /// their descriptors, and every later lease or residency check finds it
    /// absent and refuses it as quarantined.
    pub fn scrub_all(&self) -> anyhow::Result<LargeObjectScrubReport> {
        let mut report = LargeObjectScrubReport::default();
        for entry in self.objects.entries_no_follow()? {
            let name = entry
                .name
                .to_str()
                .filter(|name| lillux::cas::valid_hash(name))
                .map(str::to_owned);
            let (lillux::PinnedEntryType::Regular, Some(name)) = (entry.entry_type, name) else {
                report
                    .findings
                    .push(LargeObjectIntegrityFinding::ForeignEntry {
                        name: escaped_entry_name(&entry.name),
                        entry_type: entry_type_name(entry.entry_type).to_owned(),
                    });
                continue;
            };
            let scrubbed = self.scrub_leased(&name, true)?;
            if scrubbed.quarantined {
                report.quarantined.push(name.clone());
            }
            if scrubbed.findings.is_empty() {
                report.objects_verified += 1;
                if let Ok(Some(size)) = self.object_size(&name) {
                    report.bytes_verified = report.bytes_verified.saturating_add(size);
                }
            } else {
                report.findings.extend(scrubbed.findings);
            }
        }
        Ok(report)
    }

    /// Verify one object and report what is wrong with it. This never moves
    /// anything: import-time payload checks use it as a pure inspection.
    pub fn scrub_object(
        &self,
        file_sha256: &str,
    ) -> anyhow::Result<Vec<LargeObjectIntegrityFinding>> {
        Ok(self.scrub_leased(file_sha256, false)?.findings)
    }

    fn scrub_leased(&self, file_sha256: &str, quarantine: bool) -> anyhow::Result<ScrubbedObject> {
        require_store_hash(file_sha256)?;
        let mut findings = Vec::new();
        let lease = open_registered_lock(|| {
            self.leases
                .open_regular_create(OsStr::new(file_sha256), true, false, 0o600)
        })?;
        flock_shared_blocking(&lease)?;
        let Some(mut object) = self.objects.open_regular(OsStr::new(file_sha256), false)? else {
            return Ok(ScrubbedObject {
                findings: vec![LargeObjectIntegrityFinding::UnreadableObject {
                    file_sha256: file_sha256.to_string(),
                    error: "absent while leased for scrub".to_string(),
                }],
                quarantined: false,
            });
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
                    error: format!("{error:#}"),
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
                    return Ok(ScrubbedObject {
                        findings,
                        quarantined: false,
                    });
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
        let quarantined = quarantine
            && findings
                .iter()
                .any(LargeObjectIntegrityFinding::is_byte_damage)
            && self.quarantine_object(file_sha256, &object)?;
        drop(lease);
        Ok(ScrubbedObject {
            findings,
            quarantined,
        })
    }

    /// Move a byte-damaged object, then its sidecar, out of the served
    /// namespaces into quarantine under the first free copy name. The object
    /// moves first: from that instant no lease can bind it. Returns false
    /// when the name no longer holds the scrubbed inode.
    fn quarantine_object(&self, file_sha256: &str, object: &fs::File) -> anyhow::Result<bool> {
        let name = OsStr::new(file_sha256);
        match self.objects.open_regular(name, false)? {
            Some(current) if same_inode(&current, object)? => {}
            _ => return Ok(false),
        }
        let quarantine = self
            .root
            .open_or_create_child(OsStr::new(QUARANTINE_DIR), 0o700)?;
        let quarantined_objects =
            quarantine.open_or_create_child(OsStr::new(OBJECTS_DIR), 0o700)?;
        let quarantined_sidecars =
            quarantine.open_or_create_child(OsStr::new(SIDECARS_DIR), 0o700)?;
        let copy = free_quarantine_name(&quarantined_objects, file_sha256)?;
        quarantined_objects
            .rename_regular_from_atomic(OsStr::new(&copy), &self.objects, name, object)
            .map_err(anyhow::Error::from)
            .with_context(|| format!("quarantining large object {file_sha256}"))?;
        self.objects.sync()?;
        quarantined_objects.sync()?;
        if let Some(sidecar) = self.sidecars.open_regular(name, false)? {
            quarantined_sidecars
                .rename_regular_from_atomic(OsStr::new(&copy), &self.sidecars, name, &sidecar)
                .map_err(anyhow::Error::from)
                .with_context(|| {
                    format!("quarantining the sidecar of large object {file_sha256}")
                })?;
            self.sidecars.sync()?;
            quarantined_sidecars.sync()?;
        }
        quarantine.sync()?;
        Ok(true)
    }

    /// Remove (or, for `dry_run`, count) every quarantined copy and sidecar.
    /// This is the operator's explicit discard of scrub evidence, run with
    /// the node stopped; nothing else ever deletes from quarantine. An entry
    /// the store did not write refuses the purge rather than being removed.
    pub fn purge_quarantine(
        &self,
        dry_run: bool,
    ) -> anyhow::Result<LargeObjectQuarantinePurgeReport> {
        let mut report = LargeObjectQuarantinePurgeReport {
            dry_run,
            ..LargeObjectQuarantinePurgeReport::default()
        };
        for (child, is_objects) in [(OBJECTS_DIR, true), (SIDECARS_DIR, false)] {
            let Some(directory) = self.quarantine_child(child)? else {
                continue;
            };
            for entry in directory.entries_no_follow()? {
                if entry.entry_type != lillux::PinnedEntryType::Regular {
                    bail!(
                        "large-object quarantine {child} holds a non-regular entry {}; remove it by hand",
                        escaped_entry_name(&entry.name)
                    );
                }
                let Some(file) = directory.open_regular(&entry.name, false)? else {
                    continue;
                };
                let bytes = file.metadata()?.len();
                if !dry_run {
                    directory.remove_if_same(&entry.name, &file)?;
                }
                if is_objects {
                    report.objects = report.objects.saturating_add(1);
                    report.bytes = report.bytes.saturating_add(bytes);
                } else {
                    report.sidecars = report.sidecars.saturating_add(1);
                }
            }
            if !dry_run {
                directory.sync()?;
            }
        }
        Ok(report)
    }
}

struct ScrubbedObject {
    findings: Vec<LargeObjectIntegrityFinding>,
    quarantined: bool,
}

/// The first copy name under which `file_sha256` is not yet quarantined: the
/// bare hash, then `<hash>.<n>`. Evidence is never overwritten.
fn free_quarantine_name(
    quarantined_objects: &lillux::PinnedDirectory,
    file_sha256: &str,
) -> anyhow::Result<String> {
    if quarantined_objects
        .entry_no_follow(OsStr::new(file_sha256))?
        .is_none()
    {
        return Ok(file_sha256.to_owned());
    }
    for copy in 1..=MAX_QUARANTINED_COPIES {
        let name = format!("{file_sha256}.{copy}");
        if quarantined_objects
            .entry_no_follow(OsStr::new(&name))?
            .is_none()
        {
            return Ok(name);
        }
    }
    bail!(
        "large-object quarantine already holds {MAX_QUARANTINED_COPIES} damaged copies of \
         {file_sha256}; run `ryeos node large-objects purge-quarantine --confirm` on the stopped node"
    )
}

fn same_inode(left: &fs::File, right: &fs::File) -> anyhow::Result<bool> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let left = left.metadata()?;
        let right = right.metadata()?;
        Ok(left.dev() == right.dev() && left.ino() == right.ino())
    }
    #[cfg(not(unix))]
    {
        let _ = (left, right);
        bail!("large-object inode identity is unavailable on this platform")
    }
}

fn staging_key_for_source(source: PinnedLargeObjectSourceIdentity) -> anyhow::Result<String> {
    Ok(lillux::cas::sha256_hex(
        format!("{}\n{}", serde_json::to_string(&source.file)?, source.size).as_bytes(),
    ))
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

/// sha256 of `len` bytes starting at `start`, streamed.
fn hash_file_range(file: &mut fs::File, start: u64, len: u64) -> anyhow::Result<String> {
    file.seek(SeekFrom::Start(start))?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0u8; INGEST_IO_BUFFER_BYTES];
    let mut remaining = len;
    while remaining > 0 {
        let want = usize::try_from(remaining.min(INGEST_IO_BUFFER_BYTES as u64))?;
        read_exact_len(file, &mut buffer[..want])?;
        digest.update(&buffer[..want]);
        remaining -= want as u64;
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn read_exact_len(file: &mut fs::File, buffer: &mut [u8]) -> anyhow::Result<()> {
    file.read_exact(buffer)
        .map_err(|error| anyhow::anyhow!("short read while verifying staged ingest: {error}"))
}

/// An entry name for a finding: UTF-8 as is, other bytes escaped.
fn escaped_entry_name(name: &OsStr) -> String {
    match name.to_str() {
        Some(name) => name.to_owned(),
        None => {
            use std::os::unix::ffi::OsStrExt as _;
            name.as_bytes()
                .iter()
                .flat_map(|byte| std::ascii::escape_default(*byte))
                .map(char::from)
                .collect()
        }
    }
}

fn entry_type_name(entry_type: lillux::PinnedEntryType) -> &'static str {
    match entry_type {
        lillux::PinnedEntryType::Directory => "directory",
        lillux::PinnedEntryType::Regular => "regular",
        lillux::PinnedEntryType::Symlink => "symlink",
        lillux::PinnedEntryType::CharacterDevice => "character_device",
        lillux::PinnedEntryType::BlockDevice => "block_device",
        lillux::PinnedEntryType::Fifo => "fifo",
        lillux::PinnedEntryType::Socket => "socket",
        lillux::PinnedEntryType::Other => "other",
    }
}

fn require_store_hash(value: &str) -> anyhow::Result<()> {
    if !lillux::cas::valid_hash(value) {
        bail!("large-object store names are 64-hex sha256 digests; got {value:?}");
    }
    Ok(())
}

/// Open a lock file under a fork-sensitive lease and register it so every
/// direct-attachment fork child closes it; the lease ends before any lock wait.
#[cfg(unix)]
fn open_registered_lock(
    open: impl FnOnce() -> anyhow::Result<fs::File>,
) -> anyhow::Result<lillux::locks::ForkClosedLockFile> {
    let lease = lillux::try_retain_fork_sensitive_descriptors()?;
    let file = open()?;
    let file = lillux::locks::ForkClosedLockFile::register(file, &lease);
    drop(lease);
    Ok(file)
}

/// Like `open_registered_lock`, for an open that yields nothing when the file
/// is absent.
#[cfg(unix)]
fn open_registered_lock_if_present(
    open: impl FnOnce() -> anyhow::Result<Option<fs::File>>,
) -> anyhow::Result<Option<lillux::locks::ForkClosedLockFile>> {
    let lease = lillux::try_retain_fork_sensitive_descriptors()?;
    let Some(file) = open()? else {
        return Ok(None);
    };
    let file = lillux::locks::ForkClosedLockFile::register(file, &lease);
    drop(lease);
    Ok(Some(file))
}

#[cfg(unix)]
fn flock_exclusive_blocking(file: &lillux::locks::ForkClosedLockFile) -> anyhow::Result<()> {
    file.lock_exclusive()?;
    Ok(())
}

#[cfg(unix)]
fn flock_shared_blocking(file: &lillux::locks::ForkClosedLockFile) -> anyhow::Result<()> {
    file.lock_shared()?;
    Ok(())
}

/// True when the exclusive lock was won without blocking.
#[cfg(unix)]
fn flock_exclusive_nonblocking(file: &lillux::locks::ForkClosedLockFile) -> anyhow::Result<bool> {
    Ok(file.try_lock_exclusive()?)
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
        let file = fs::File::open(source)?;
        let metadata = file.metadata()?;
        let identity = PinnedLargeObjectSourceIdentity {
            file: lillux::pinned_regular_file_identity(&file)?,
            size: metadata.len(),
        };
        store.ingest_open_regular(
            file,
            identity,
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
        let metadata = fs::metadata(&source).unwrap();
        let staging_key = staging_key_for_source(PinnedLargeObjectSourceIdentity {
            file: lillux::pinned_regular_file_identity(&fs::File::open(&source).unwrap()).unwrap(),
            size: metadata.len(),
        })
        .unwrap();
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
            file: lillux::pinned_regular_file_identity(&fs::File::open(&source2).unwrap()).unwrap(),
            size: metadata.len(),
        })
        .unwrap();
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
    fn scrub_reports_foreign_entries_and_still_verifies_real_objects() {
        use std::os::unix::ffi::OsStrExt as _;
        let (dir, store) = store(8);
        let ingested =
            ingest(&store, &write_source(&dir, "payload.bin", &[6u8; 24]), None).unwrap();
        let objects = dir.path().join("large-objects/objects");
        fs::write(objects.join("not-a-hash"), b"stray").unwrap();
        fs::create_dir(objects.join("sub")).unwrap();
        std::os::unix::fs::symlink(&ingested.file_sha256, objects.join("link")).unwrap();
        fs::write(objects.join(OsStr::from_bytes(b"bad\xff")), b"stray").unwrap();

        let report = store.scrub_all().unwrap();
        assert_eq!(report.objects_verified, 1);
        assert_eq!(report.bytes_verified, 24);
        let mut foreign = report
            .findings
            .iter()
            .map(|finding| match finding {
                LargeObjectIntegrityFinding::ForeignEntry { name, entry_type } => {
                    (name.as_str(), entry_type.as_str())
                }
                other => panic!("unexpected finding {other:?}"),
            })
            .collect::<Vec<_>>();
        foreign.sort();
        assert_eq!(
            foreign,
            [
                ("bad\\xff", "regular"),
                ("link", "symlink"),
                ("not-a-hash", "regular"),
                ("sub", "directory"),
            ]
        );
    }

    /// Flip one byte of a published object in place, as disk rot or a
    /// root-owned writer would; the published mode is 0444.
    fn corrupt_object(dir: &tempfile::TempDir, file_sha256: &str, at: usize) {
        use std::io::{Read as _, Seek as _, Write as _};
        use std::os::unix::fs::PermissionsExt as _;
        let object_path = dir.path().join("large-objects/objects").join(file_sha256);
        let mut permissions = fs::metadata(&object_path).unwrap().permissions();
        permissions.set_mode(0o644);
        fs::set_permissions(&object_path, permissions).unwrap();
        let mut file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&object_path)
            .unwrap();
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).unwrap();
        bytes[at] ^= 0xff;
        file.seek(SeekFrom::Start(0)).unwrap();
        file.write_all(&bytes).unwrap();
        file.sync_all().unwrap();
    }

    #[test]
    fn scrub_quarantines_a_byte_damaged_object_and_its_sidecar() {
        let (dir, store) = store(8);
        let bytes = vec![7u8; 24];
        let ingested = ingest(&store, &write_source(&dir, "payload.bin", &bytes), None).unwrap();
        let hash = ingested.file_sha256.clone();
        corrupt_object(&dir, &hash, 9);

        let report = store.scrub_all().unwrap();
        assert_eq!(report.quarantined, vec![hash.clone()]);
        assert_eq!(report.objects_verified, 0);
        assert!(
            report.findings.iter().any(|finding| matches!(
                finding,
                LargeObjectIntegrityFinding::FileHashMismatch { .. }
            )),
            "got: {:?}",
            report.findings
        );
        let root = dir.path().join("large-objects");
        assert!(!root.join("objects").join(&hash).exists());
        assert!(!root.join("sidecars").join(&hash).exists());
        let kept = fs::read(root.join("quarantine/objects").join(&hash)).unwrap();
        assert_ne!(kept, bytes, "the damaged bytes are kept as evidence");
        assert!(root.join("quarantine/sidecars").join(&hash).exists());
        assert_eq!(store.object_size(&hash).unwrap(), None);
        assert!(store.is_quarantined(&hash).unwrap());

        // A second scrub finds nothing left to verify or move.
        let again = store.scrub_all().unwrap();
        assert!(again.quarantined.is_empty());
        assert!(again.findings.is_empty());
    }

    #[test]
    fn a_quarantined_object_refuses_typed_and_reingest_publishes_clean_bytes() {
        let (dir, store) = store(8);
        let bytes = pseudo_bytes(40);
        let source = write_source(&dir, "payload.bin", &bytes);
        let ingested = ingest(&store, &source, None).unwrap();
        let hash = ingested.file_sha256.clone();
        corrupt_object(&dir, &hash, 3);
        assert_eq!(store.scrub_all().unwrap().quarantined, vec![hash.clone()]);

        for error in [
            store.lease_object(&hash, 40).err().unwrap(),
            store.verify_resident_object(&hash, 40).unwrap_err(),
        ] {
            let quarantined = error
                .downcast_ref::<LargeObjectQuarantined>()
                .unwrap_or_else(|| panic!("untyped refusal: {error:#}"));
            assert_eq!(quarantined.file_sha256, hash);
            let message = error.to_string();
            assert!(message.contains("Re-import"), "{message}");
            assert!(message.contains("purge-quarantine"), "{message}");
        }
        assert!(store.try_lease_object(&hash, 40).unwrap().is_none());

        // The name is free, so the right bytes publish as new rather than
        // deduplicating against the damaged copy.
        let source = write_source(&dir, "payload-again.bin", &bytes);
        let reingested = ingest(&store, &source, Some(hash.as_str())).unwrap();
        assert!(!reingested.deduplicated);
        assert!(store.scrub_object(&hash).unwrap().is_empty());
        store.verify_resident_object(&hash, 40).unwrap();
        assert_eq!(
            fs::read(dir.path().join("large-objects/objects").join(&hash)).unwrap(),
            bytes
        );
        // The evidence stays until an operator purges it.
        assert!(store.is_quarantined(&hash).unwrap());
    }

    #[test]
    fn an_upload_of_a_quarantined_hash_publishes_clean_bytes() {
        let (dir, store) = store(8);
        let bytes = pseudo_bytes(24);
        let (hash, expect) = expectation_for(&bytes, 8);
        let first = feed(&store, &hash, &expect, &bytes, 8).unwrap();
        assert!(first.complete);
        corrupt_object(&dir, &hash, 0);
        assert_eq!(store.scrub_all().unwrap().quarantined, vec![hash.clone()]);
        assert_eq!(store.staged_bytes(&hash).unwrap(), 0);

        let again = feed(&store, &hash, &expect, &bytes, 8).unwrap();
        assert!(again.complete);
        assert!(!again.deduplicated);
        assert!(store.scrub_object(&hash).unwrap().is_empty());
    }

    #[test]
    fn a_lease_held_across_quarantine_keeps_reading_its_inode() {
        let (dir, store) = store(8);
        let ingested =
            ingest(&store, &write_source(&dir, "payload.bin", &[8u8; 24]), None).unwrap();
        let hash = ingested.file_sha256.clone();
        use std::io::{Read as _, Seek as _};
        let held = store.lease_object(&hash, 24).unwrap();
        corrupt_object(&dir, &hash, 1);
        assert_eq!(store.scrub_all().unwrap().quarantined, vec![hash.clone()]);
        let mut file = held.file().try_clone().unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        let mut read = Vec::new();
        file.read_to_end(&mut read).unwrap();
        assert_eq!(read.len(), 24);
        assert_eq!(store.object_size(&hash).unwrap(), None);
    }

    #[test]
    fn sidecar_damage_is_reported_not_quarantined() {
        let (dir, store) = store(8);
        let ingested =
            ingest(&store, &write_source(&dir, "payload.bin", &[9u8; 24]), None).unwrap();
        let hash = ingested.file_sha256.clone();
        fs::remove_file(dir.path().join("large-objects/sidecars").join(&hash)).unwrap();
        let report = store.scrub_all().unwrap();
        assert!(report.quarantined.is_empty());
        assert!(
            report.findings.iter().any(|finding| matches!(
                finding,
                LargeObjectIntegrityFinding::MissingSidecar { .. }
            )),
            "got: {:?}",
            report.findings
        );
        assert_eq!(store.object_size(&hash).unwrap(), Some(24));
        assert!(!store.is_quarantined(&hash).unwrap());
        assert!(!dir.path().join("large-objects/quarantine").exists());
    }

    #[test]
    fn scrub_object_inspects_without_quarantining() {
        let (dir, store) = store(8);
        let ingested = ingest(
            &store,
            &write_source(&dir, "payload.bin", &[10u8; 24]),
            None,
        )
        .unwrap();
        let hash = ingested.file_sha256.clone();
        corrupt_object(&dir, &hash, 2);
        let findings = store.scrub_object(&hash).unwrap();
        assert!(
            findings
                .iter()
                .any(LargeObjectIntegrityFinding::is_byte_damage)
        );
        assert_eq!(store.object_size(&hash).unwrap(), Some(24));
        assert!(!store.is_quarantined(&hash).unwrap());
    }

    #[test]
    fn quarantined_bytes_count_toward_the_budget_but_are_never_evicted() {
        let (dir, store) = store(8);
        let damaged = ingest(
            &store,
            &write_source(&dir, "damaged.bin", &[11u8; 32]),
            None,
        )
        .unwrap();
        let healthy = ingest(
            &store,
            &write_source(&dir, "healthy.bin", &[12u8; 16]),
            None,
        )
        .unwrap();
        corrupt_object(&dir, &damaged.file_sha256, 0);
        store.scrub_all().unwrap();
        assert_eq!(store.quarantined_bytes().unwrap(), 32);
        assert_eq!(store.total_stored_bytes().unwrap(), 48);

        let report = store.sweep_to_budget(0, &BTreeSet::new()).unwrap();
        assert_eq!(report.quarantined_bytes, 32);
        assert_eq!(report.total_bytes_before, 48);
        assert_eq!(
            report.evicted,
            vec![(healthy.file_sha256.clone(), 16)],
            "only the served object is a candidate"
        );
        assert_eq!(report.total_bytes_after, 32);
        assert!(store.is_quarantined(&damaged.file_sha256).unwrap());
        assert_eq!(store.total_stored_bytes().unwrap(), 32);
    }

    #[test]
    fn a_second_quarantine_of_the_same_hash_keeps_the_first_copy() {
        let (dir, store) = store(8);
        let bytes = vec![13u8; 24];
        let hash = ingest(&store, &write_source(&dir, "one.bin", &bytes), None)
            .unwrap()
            .file_sha256;
        corrupt_object(&dir, &hash, 0);
        store.scrub_all().unwrap();
        ingest(
            &store,
            &write_source(&dir, "two.bin", &bytes),
            Some(hash.as_str()),
        )
        .unwrap();
        corrupt_object(&dir, &hash, 5);
        assert_eq!(store.scrub_all().unwrap().quarantined, vec![hash.clone()]);
        let quarantine = dir.path().join("large-objects/quarantine/objects");
        let first = fs::read(quarantine.join(&hash)).unwrap();
        let second = fs::read(quarantine.join(format!("{hash}.1"))).unwrap();
        assert_ne!(first, second);
        assert!(
            dir.path()
                .join("large-objects/quarantine/sidecars")
                .join(format!("{hash}.1"))
                .exists()
        );
        assert_eq!(store.quarantined_bytes().unwrap(), 48);
    }

    #[test]
    fn purge_quarantine_previews_then_removes_only_the_evidence() {
        let (dir, store) = store(8);
        let damaged = ingest(
            &store,
            &write_source(&dir, "damaged.bin", &[14u8; 24]),
            None,
        )
        .unwrap();
        let healthy = ingest(
            &store,
            &write_source(&dir, "healthy.bin", &[15u8; 16]),
            None,
        )
        .unwrap();
        assert_eq!(
            store.purge_quarantine(false).unwrap(),
            LargeObjectQuarantinePurgeReport::default()
        );
        corrupt_object(&dir, &damaged.file_sha256, 0);
        store.scrub_all().unwrap();

        let preview = store.purge_quarantine(true).unwrap();
        assert_eq!(
            preview,
            LargeObjectQuarantinePurgeReport {
                dry_run: true,
                objects: 1,
                bytes: 24,
                sidecars: 1,
            }
        );
        assert!(store.is_quarantined(&damaged.file_sha256).unwrap());

        let purged = store.purge_quarantine(false).unwrap();
        assert_eq!(purged.objects, 1);
        assert_eq!(purged.bytes, 24);
        assert!(!purged.dry_run);
        assert!(!store.is_quarantined(&damaged.file_sha256).unwrap());
        assert_eq!(store.quarantined_bytes().unwrap(), 0);
        assert_eq!(store.object_size(&healthy.file_sha256).unwrap(), Some(16));
        // Absent and no longer quarantined: an ordinary absence.
        let error = store
            .verify_resident_object(&damaged.file_sha256, 24)
            .unwrap_err();
        assert!(error.downcast_ref::<LargeObjectQuarantined>().is_none());

        fs::create_dir(dir.path().join("large-objects/quarantine/objects/stray")).unwrap();
        assert!(store.purge_quarantine(true).is_err());
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

    const MIB: u64 = 1024 * 1024;

    fn pseudo_bytes(len: usize) -> Vec<u8> {
        (0..len)
            .map(|i| (i.wrapping_mul(31) ^ (i >> 9)) as u8)
            .collect()
    }

    fn expectation_for(bytes: &[u8], chunk: u64) -> (String, LargeObjectExpectation) {
        let mut hasher = ChunkedHasher::new(chunk);
        hasher.update(bytes);
        let (file_sha256, chunk_hashes) = hasher.finish();
        (
            file_sha256,
            LargeObjectExpectation {
                size: bytes.len() as u64,
                chunk_size: chunk,
                chunk_hashes,
            },
        )
    }

    fn staging_path(dir: &tempfile::TempDir, file_sha256: &str) -> PathBuf {
        dir.path()
            .join("large-objects/.staging")
            .join(format!("upload-{file_sha256}"))
    }

    /// Feed `bytes` in `piece`-sized writes; returns the last progress.
    fn feed(
        store: &LargeObjectStore,
        file_sha256: &str,
        expect: &LargeObjectExpectation,
        bytes: &[u8],
        piece: usize,
    ) -> anyhow::Result<StageProgress> {
        let mut offset = 0usize;
        let mut last = None;
        while offset < bytes.len() {
            let end = (offset + piece).min(bytes.len());
            last =
                Some(store.stage_chunk(file_sha256, expect, offset as u64, &bytes[offset..end])?);
            offset = end;
        }
        Ok(last.unwrap())
    }

    #[test]
    fn staged_upload_publishes_the_same_sidecar_as_a_local_ingest() {
        let bytes = pseudo_bytes(3 * MIB as usize + 12_345);
        let (file_sha256, expect) = expectation_for(&bytes, MIB);

        let (_dir, staged_store) = store(MIB);
        let progress = feed(&staged_store, &file_sha256, &expect, &bytes, 700 * 1024).unwrap();
        assert_eq!(
            progress,
            StageProgress {
                staged_bytes: bytes.len() as u64,
                complete: true,
                deduplicated: false,
            }
        );

        let (ingest_dir, ingest_store) = store(MIB);
        let source = write_source(&ingest_dir, "payload.bin", &bytes);
        let ingested = ingest(&ingest_store, &source, None).unwrap();
        assert_eq!(ingested.file_sha256, file_sha256);
        assert_eq!(ingested.chunk_hashes, expect.chunk_hashes);

        assert_eq!(
            staged_store.sidecar(&file_sha256).unwrap(),
            ingest_store.sidecar(&file_sha256).unwrap()
        );
        assert!(staged_store.sidecar(&file_sha256).unwrap().is_some());
        assert_eq!(
            staged_store.object_size(&file_sha256).unwrap(),
            Some(bytes.len() as u64)
        );
        staged_store
            .verify_resident_object(&file_sha256, bytes.len() as u64)
            .unwrap();
        assert_eq!(
            staged_store.staged_bytes(&file_sha256).unwrap(),
            bytes.len() as u64
        );
    }

    #[test]
    fn the_same_chunk_twice_is_idempotent() {
        let bytes = pseudo_bytes(2 * MIB as usize);
        let (file_sha256, expect) = expectation_for(&bytes, MIB);
        let (_dir, store) = store(MIB);
        let first = store
            .stage_chunk(&file_sha256, &expect, 0, &bytes[..512 * 1024])
            .unwrap();
        let again = store
            .stage_chunk(&file_sha256, &expect, 0, &bytes[..512 * 1024])
            .unwrap();
        assert_eq!(first, again);
        assert_eq!(first.staged_bytes, 512 * 1024);
        assert!(!first.complete);
        assert_eq!(store.staged_bytes(&file_sha256).unwrap(), 512 * 1024);
    }

    #[test]
    fn an_out_of_order_offset_is_refused_naming_the_expected_offset() {
        let bytes = pseudo_bytes(2 * MIB as usize);
        let (file_sha256, expect) = expectation_for(&bytes, MIB);
        let (_dir, store) = store(MIB);
        let error = store
            .stage_chunk(&file_sha256, &expect, 4096, &bytes[4096..8192])
            .unwrap_err();
        assert!(error.to_string().contains("expected offset 0"), "{error}");
        store
            .stage_chunk(&file_sha256, &expect, 0, &bytes[..8192])
            .unwrap();
        let error = store
            .stage_chunk(&file_sha256, &expect, 16_384, &bytes[16_384..20_480])
            .unwrap_err();
        assert!(
            error.to_string().contains("expected offset 8192"),
            "{error}"
        );
        let error = store
            .stage_chunk(&file_sha256, &expect, 2 * MIB - 4, &bytes[..16])
            .unwrap_err();
        assert!(error.to_string().contains("extends past"), "{error}");
    }

    #[test]
    fn a_retried_write_extends_the_stage_but_a_differing_prefix_is_refused() {
        let bytes = pseudo_bytes(2 * MIB as usize);
        let (file_sha256, expect) = expectation_for(&bytes, MIB);
        let (_dir, store) = store(MIB);
        store
            .stage_chunk(&file_sha256, &expect, 0, &bytes[..512 * 1024])
            .unwrap();

        let overlapping = store
            .stage_chunk(
                &file_sha256,
                &expect,
                256 * 1024,
                &bytes[256 * 1024..768 * 1024],
            )
            .unwrap();
        assert_eq!(overlapping.staged_bytes, 768 * 1024);

        let mut altered = bytes[100..200].to_vec();
        altered[50] ^= 0xff;
        let error = store
            .stage_chunk(&file_sha256, &expect, 100, &altered)
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("retried bytes differ from the staged bytes at offset 150"),
            "{error}"
        );
        assert_eq!(store.staged_bytes(&file_sha256).unwrap(), 768 * 1024);
    }

    #[test]
    fn a_wrong_sidecar_chunk_hash_is_refused_and_the_chunk_is_cut_off() {
        let bytes = pseudo_bytes(3 * MIB as usize);
        let (file_sha256, mut expect) = expectation_for(&bytes, MIB);
        expect.chunk_hashes[1] = lillux::cas::sha256_hex(b"not the second chunk");
        let (dir, store) = store(MIB);
        store
            .stage_chunk(&file_sha256, &expect, 0, &bytes[..MIB as usize])
            .unwrap();
        store
            .stage_chunk(
                &file_sha256,
                &expect,
                MIB,
                &bytes[MIB as usize..MIB as usize + 4096],
            )
            .unwrap();
        let error = store
            .stage_chunk(
                &file_sha256,
                &expect,
                MIB + 4096,
                &bytes[MIB as usize + 4096..2 * MIB as usize],
            )
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("chunk 1 of the large object failed its hash"),
            "{error}"
        );
        assert_eq!(store.staged_bytes(&file_sha256).unwrap(), MIB);
        assert_eq!(
            fs::metadata(staging_path(&dir, &file_sha256))
                .unwrap()
                .len(),
            MIB
        );
        assert_eq!(store.object_size(&file_sha256).unwrap(), None);
    }

    #[test]
    fn a_wrong_whole_file_hash_is_refused_and_the_staging_removed() {
        let bytes = pseudo_bytes(2 * MIB as usize);
        let (_real_sha256, expect) = expectation_for(&bytes, MIB);
        let wrong_sha256 = lillux::cas::sha256_hex(b"some other file");
        let (dir, store) = store(MIB);
        let error = feed(&store, &wrong_sha256, &expect, &bytes, MIB as usize).unwrap_err();
        assert!(error.to_string().contains("expected"), "{error}");
        assert!(!staging_path(&dir, &wrong_sha256).exists());
        assert_eq!(store.staged_bytes(&wrong_sha256).unwrap(), 0);
        assert_eq!(store.object_size(&wrong_sha256).unwrap(), None);
    }

    #[test]
    fn a_resident_object_short_circuits_without_writing() {
        let bytes = pseudo_bytes(MIB as usize + 77);
        let (file_sha256, expect) = expectation_for(&bytes, MIB);
        let (dir, store) = store(MIB);
        let source = write_source(&dir, "payload.bin", &bytes);
        ingest(&store, &source, None).unwrap();

        let progress = store
            .stage_chunk(&file_sha256, &expect, 0, &bytes[..4096])
            .unwrap();
        assert_eq!(
            progress,
            StageProgress {
                staged_bytes: bytes.len() as u64,
                complete: true,
                deduplicated: true,
            }
        );
        assert!(!staging_path(&dir, &file_sha256).exists());
        assert_eq!(
            store.staged_bytes(&file_sha256).unwrap(),
            bytes.len() as u64
        );
    }

    #[test]
    fn an_upload_resumes_after_the_store_is_reopened() {
        let bytes = pseudo_bytes(3 * MIB as usize + 5);
        let (file_sha256, expect) = expectation_for(&bytes, MIB);
        let (dir, first) = store(MIB);
        let half = 1_500_000usize;
        first
            .stage_chunk(&file_sha256, &expect, 0, &bytes[..half])
            .unwrap();
        drop(first);

        let runtime = lillux::PinnedDirectory::open_or_create(dir.path()).unwrap();
        let reopened = LargeObjectStore::open_with_chunk_bytes(&runtime, MIB).unwrap();
        assert_eq!(reopened.staged_bytes(&file_sha256).unwrap(), half as u64);
        let progress = reopened
            .stage_chunk(&file_sha256, &expect, half as u64, &bytes[half..])
            .unwrap();
        assert!(progress.complete);
        assert!(!progress.deduplicated);
        reopened
            .verify_resident_object(&file_sha256, bytes.len() as u64)
            .unwrap();
    }

    #[test]
    fn a_swept_abandoned_upload_restarts_at_zero() {
        let bytes = pseudo_bytes(2 * MIB as usize);
        let (file_sha256, expect) = expectation_for(&bytes, MIB);
        let (_dir, store) = store(MIB);
        store
            .stage_chunk(&file_sha256, &expect, 0, &bytes[..MIB as usize])
            .unwrap();
        assert_eq!(store.sweep_abandoned_staging().unwrap(), 1);
        assert_eq!(store.staged_bytes(&file_sha256).unwrap(), 0);
        let error = store
            .stage_chunk(&file_sha256, &expect, MIB, &bytes[MIB as usize..])
            .unwrap_err();
        assert!(error.to_string().contains("expected offset 0"), "{error}");
        let progress = feed(&store, &file_sha256, &expect, &bytes, MIB as usize).unwrap();
        assert!(progress.complete);
    }

    #[test]
    fn a_contradictory_expectation_is_refused_before_any_write() {
        let bytes = pseudo_bytes(2 * MIB as usize);
        let (file_sha256, expect) = expectation_for(&bytes, MIB);
        let (dir, store) = store(MIB);
        let mut wrong_chunk = expect.clone();
        wrong_chunk.chunk_size = 2 * MIB;
        assert!(
            store
                .stage_chunk(&file_sha256, &wrong_chunk, 0, &bytes[..16])
                .is_err()
        );
        let mut wrong_count = expect.clone();
        wrong_count.chunk_hashes.pop();
        assert!(
            store
                .stage_chunk(&file_sha256, &wrong_count, 0, &bytes[..16])
                .is_err()
        );
        let mut empty = expect.clone();
        empty.size = 0;
        assert!(
            store
                .stage_chunk(&file_sha256, &empty, 0, &bytes[..16])
                .is_err()
        );
        assert!(
            store
                .stage_chunk("not-a-hash", &expect, 0, &bytes[..16])
                .is_err()
        );
        assert!(!staging_path(&dir, &file_sha256).exists());
    }
}
