//! Bundle staging: the node-owned holding area for bundles that were fetched
//! and verified but are not installed.
//!
//! A staged bundle is inert bytes. It lives under `<state>/bundle-staging/`,
//! outside every bundle root and every path the engine loads, so it is never
//! registered, never on a search path and never reported as installed. Staging
//! confers no authority to activate anything: a later signed operation reads a
//! stage and decides.
//!
//! Layout:
//!
//! ```text
//! <state>/bundle-staging/
//!   <tree digest>/            one committed stage
//!     tree/                   the verified bundle files
//!     <evidence files>        node-verified source documents kept for a later
//!                             re-verification (publication record, manifest,
//!                             the origin node's public key document)
//!     stage.record            node-signed staging record, written last
//!   .tmp-<random>/            a stage under construction
//! ```
//!
//! A stage is built in a temporary directory and renamed to its digest name;
//! the record is written last. A crash leaves nothing or a complete stage: a
//! digest directory with no readable record is not listed, and the next commit
//! removes it. A refused stage removes its temporary directory and writes no
//! record.
//!
//! The record is signed by the node key because it is the node's testimony
//! that these bytes were verified. Its `staged_at` is the node's own claim;
//! nothing orders by it. It carries no prediction of what activation would do:
//! the facets it lists are what the bundle declares, not what any policy would
//! admit.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use lillux::crypto::{SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use serde_json::json;

/// Schema tag of the staging record body.
pub const STAGING_SCHEMA: &str = "ryeos.bundle-staging/v1";
/// Directory below the node's state directory that holds every stage.
pub const STAGING_DIR: &str = "bundle-staging";
/// The record file inside a committed stage.
pub const RECORD_FILE: &str = "stage.record";
/// The verified bundle tree inside a committed stage.
pub const TREE_DIR: &str = "tree";
/// The status a staged bundle reports.
pub const STATUS_STAGED: &str = "staged";
/// What a pull or a remote install tells its caller. A stage is never an
/// install.
pub const STAGED_RECEIPT: &str =
    "fetched and verified; staged; takes effect at the next start by a signed operation";

/// Source kind of a stage fetched from a published record.
pub const SOURCE_PUBLICATION: &str = "publication";
/// Source kind of a stage fetched from a configured remote node.
pub const SOURCE_REMOTE: &str = "remote";

const TEMP_PREFIX: &str = ".tmp-";
const MAX_RECORD_BYTES: u64 = 1024 * 1024;
const MAX_EVIDENCE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_EVIDENCE_FILES: usize = 8;
/// A temporary directory this old belongs to a transfer that is gone.
const STALE_TEMP_AGE: Duration = Duration::from_secs(24 * 60 * 60);
const SWEEP_REMOVE_LIMIT: usize = 8;

/// Default bound on all stages held by the node, in bytes.
pub const DEFAULT_MAX_NODE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// Default bound on the number of stages the node holds.
pub const DEFAULT_MAX_NODE_STAGES: usize = 64;
/// Default bound on the bytes staged on behalf of one requesting key.
pub const DEFAULT_MAX_KEY_BYTES: u64 = 512 * 1024 * 1024;
/// Default bound on the number of stages one requesting key holds.
pub const DEFAULT_MAX_KEY_STAGES: usize = 16;

/// The byte and count bounds on staging, per node and per requesting key.
///
/// The defaults are named constants: the node policy has no section for them
/// yet. Callers take the value from here so that a policy section can feed it
/// without changing the stage code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StagingCaps {
    pub max_node_bytes: u64,
    pub max_node_stages: usize,
    pub max_key_bytes: u64,
    pub max_key_stages: usize,
}

impl StagingCaps {
    pub const DEFAULTS: Self = Self {
        max_node_bytes: DEFAULT_MAX_NODE_BYTES,
        max_node_stages: DEFAULT_MAX_NODE_STAGES,
        max_key_bytes: DEFAULT_MAX_KEY_BYTES,
        max_key_stages: DEFAULT_MAX_KEY_STAGES,
    };
}

impl Default for StagingCaps {
    fn default() -> Self {
        Self::DEFAULTS
    }
}

/// Why a stage was refused. Every refusal has a stable code, and none of them
/// leaves a record or a temporary directory behind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StagingRefusal {
    /// The node's staged bytes would pass the node byte cap.
    NodeBytes {
        limit: u64,
        held: u64,
        requested: u64,
    },
    /// The node would hold more stages than the node count cap.
    NodeCount { limit: usize },
    /// The requesting key's staged bytes would pass its byte cap.
    KeyBytes {
        limit: u64,
        held: u64,
        requested: u64,
    },
    /// The requesting key would hold more stages than its count cap.
    KeyCount { limit: usize },
    /// The stage's own content is not acceptable.
    Invalid(String),
    /// The staging area could not be read or written.
    Store(String),
}

impl StagingRefusal {
    pub fn code(&self) -> &'static str {
        match self {
            Self::NodeBytes { .. } => "bundle_stage_node_bytes_cap",
            Self::NodeCount { .. } => "bundle_stage_node_count_cap",
            Self::KeyBytes { .. } => "bundle_stage_key_bytes_cap",
            Self::KeyCount { .. } => "bundle_stage_key_count_cap",
            Self::Invalid(_) => "bundle_stage_invalid",
            Self::Store(_) => "bundle_stage_store_failed",
        }
    }

    /// 507 when the node as a whole is full, 429 when the requesting key has
    /// used its own share, 422 for unacceptable content, 500 for a store fault.
    pub fn status(&self) -> u16 {
        match self {
            Self::NodeBytes { .. } | Self::NodeCount { .. } => 507,
            Self::KeyBytes { .. } | Self::KeyCount { .. } => 429,
            Self::Invalid(_) => 422,
            Self::Store(_) => 500,
        }
    }

    fn store(error: impl std::fmt::Display) -> Self {
        Self::Store(format!("{error:#}"))
    }
}

impl std::fmt::Display for StagingRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: ", self.code())?;
        match self {
            Self::NodeBytes {
                limit,
                held,
                requested,
            } => write!(
                f,
                "the node holds {held} staged bytes and this stage adds {requested}; the node cap \
                 is {limit}. Nothing was staged"
            ),
            Self::NodeCount { limit } => write!(
                f,
                "the node already holds {limit} stages, its cap. Nothing was staged"
            ),
            Self::KeyBytes {
                limit,
                held,
                requested,
            } => write!(
                f,
                "this key already has {held} staged bytes and this stage adds {requested}; the \
                 per-key cap is {limit}. Nothing was staged"
            ),
            Self::KeyCount { limit } => write!(
                f,
                "this key already has {limit} stages, the per-key cap. Nothing was staged"
            ),
            Self::Invalid(reason) | Self::Store(reason) => f.write_str(reason),
        }
    }
}

impl std::error::Error for StagingRefusal {}

/// Where a staged bundle came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StagedSource {
    /// [`SOURCE_PUBLICATION`] or [`SOURCE_REMOTE`].
    pub kind: String,
    /// A label for the origin: `publication:<id>` or `remote:<name>`.
    pub origin: String,
    /// The publication record hash the pull verified against.
    pub publication_record_hash: Option<String>,
    /// The origin node fingerprint the pull was pinned to, without `fp:`.
    pub pinned_node: Option<String>,
    /// The hash of the manifest the publication record names.
    pub manifest_hash: Option<String>,
}

/// What the bundle's own signed manifest declares. These are declarations, not
/// admissions: nothing here says what any policy would allow.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeclaredFacets {
    pub provides_kinds: Vec<String>,
    pub requires_kinds: Vec<String>,
    pub uses_kinds: Vec<String>,
}

/// The node-signed body of a staging record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StagingRecord {
    pub schema: String,
    pub bundle_name: String,
    /// The digest of the verified file list; also the stage's directory name.
    pub tree_digest: String,
    pub source: StagedSource,
    /// Fingerprint of the key whose request caused this stage, without `fp:`.
    pub requested_by: String,
    /// The node's own policy generation digest when the stage was verified.
    pub policy_generation: String,
    pub declared: DeclaredFacets,
    pub file_count: u64,
    pub total_bytes: u64,
    /// Evidence files kept beside the tree, by name, with the SHA-256 of each:
    /// the signature on this record binds them.
    pub evidence: BTreeMap<String, String>,
    /// The node's claim of when it staged this. Nothing orders by it.
    pub staged_at: String,
}

impl StagingRecord {
    fn validate(&self) -> Result<(), StagingRefusal> {
        let invalid = |reason: &str| Err(StagingRefusal::Invalid(reason.to_owned()));
        if self.schema != STAGING_SCHEMA {
            return invalid("the staging record has an unknown schema");
        }
        if !lillux::valid_hash(&self.tree_digest) {
            return invalid("the staging record's tree digest is not a SHA-256 digest");
        }
        if self.bundle_name.is_empty() {
            return invalid("the staging record names no bundle");
        }
        if self.evidence.len() > MAX_EVIDENCE_FILES
            || self
                .evidence
                .iter()
                .any(|(name, digest)| !is_evidence_name(name) || !lillux::valid_hash(digest))
        {
            return invalid("the staging record's evidence list is malformed");
        }
        Ok(())
    }
}

fn is_evidence_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name != RECORD_FILE
        && name != TREE_DIR
        && !name.starts_with('.')
        && name.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-')
        })
}

/// One verified file of a staged bundle, as the digest sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TreeFile {
    pub path: String,
    pub hash: String,
    pub size: u64,
    pub mode: u32,
}

/// The digest of a bundle's verified file list: the bundle name and every
/// path with its content hash, size and mode, in path order. Two stages share
/// a digest exactly when they hold the same bundle's same bytes.
pub fn tree_digest(bundle_name: &str, files: &[TreeFile]) -> Result<String, StagingRefusal> {
    let mut sorted: Vec<&TreeFile> = files.iter().collect();
    sorted.sort_by(|a, b| a.path.cmp(&b.path));
    let canonical = lillux::canonical_json(&json!({
        "schema": "ryeos.bundle-tree/v1",
        "bundle_name": bundle_name,
        "files": sorted,
    }))
    .map_err(StagingRefusal::store)?;
    Ok(lillux::sha256_hex(canonical.as_bytes()))
}

/// Bounds on a tree walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TreeLimits {
    pub max_files: usize,
    pub max_bytes: u64,
}

/// Deepest directory nesting a tree walk accepts.
pub const MAX_TREE_DEPTH: usize = 64;

/// What a tree walk found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeSummary {
    pub digest: String,
    pub file_count: u64,
    pub total_bytes: u64,
}

/// Compute the tree digest of `bundle_name` from a directory on disk, with the
/// same definition as [`tree_digest`] over a file list. Stage and apply both
/// call this and compare the result with the digest they expect.
///
/// The walk uses `lstat` and never follows a link. Only regular files and
/// non-empty directories are accepted: a symlink, device, fifo, socket or empty
/// directory is refused, so no entry exists that the digest does not cover.
/// Every entry must be owned by the node's effective user and no directory may
/// be group- or other-writable. Each file is hashed as a stream, its exact
/// permission bits are part of the digest input, and the file count, total size
/// and depth are bounded. The walk reads the tree alone: a file nobody named
/// simply changes the digest.
pub fn tree_digest_from_dir(
    bundle_name: &str,
    tree: &Path,
    limits: &TreeLimits,
) -> Result<TreeSummary, StagingRefusal> {
    use sha2::{Digest as _, Sha256};
    use std::io::Read as _;

    let invalid = |reason: String| StagingRefusal::Invalid(format!("the staged tree: {reason}"));
    let uid = lillux::effective_uid().map_err(StagingRefusal::store)?;
    let observe =
        |path: &Path| lillux::observe_tree_entry_no_follow(path).map_err(StagingRefusal::store);
    let mut files: Vec<TreeFile> = Vec::new();
    let mut total_bytes = 0_u64;
    let mut pending = vec![(tree.to_path_buf(), String::new(), 0_usize)];
    while let Some((dir, prefix, depth)) = pending.pop() {
        if depth > MAX_TREE_DEPTH {
            return Err(invalid(format!(
                "nests deeper than {MAX_TREE_DEPTH} directories"
            )));
        }
        let observed = observe(&dir)?;
        if observed.kind != lillux::TreeEntryKind::Directory {
            return Err(invalid(format!("{} is not a directory", dir.display())));
        }
        if observed.owner != uid {
            return Err(invalid(format!(
                "{} is not owned by the node's user",
                dir.display()
            )));
        }
        if observed.group_or_other_writable() {
            return Err(invalid(format!(
                "{} is group- or other-writable",
                dir.display()
            )));
        }
        let mut entries = 0_usize;
        for entry in std::fs::read_dir(&dir).map_err(StagingRefusal::store)? {
            let entry = entry.map_err(StagingRefusal::store)?;
            entries += 1;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| invalid("has a file name that is not UTF-8".to_owned()))?;
            let relative = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            let path = entry.path();
            match observe(&path)?.kind {
                lillux::TreeEntryKind::Directory => pending.push((path, relative, depth + 1)),
                lillux::TreeEntryKind::RegularFile => {
                    if files.len() >= limits.max_files {
                        return Err(invalid(format!(
                            "holds more than {} files",
                            limits.max_files
                        )));
                    }
                    let (mut file, opened) = lillux::open_tree_regular_file_no_follow(&path)
                        .map_err(|e| invalid(format!("{e:#}")))?;
                    if opened.owner != uid {
                        return Err(invalid(format!(
                            "{relative} is not owned by the node's user"
                        )));
                    }
                    total_bytes = total_bytes.saturating_add(opened.size);
                    if total_bytes > limits.max_bytes {
                        return Err(invalid(format!(
                            "holds more than {} bytes",
                            limits.max_bytes
                        )));
                    }
                    let mut hasher = Sha256::new();
                    let mut read_total = 0_u64;
                    let mut buffer = vec![0_u8; 64 * 1024];
                    loop {
                        let read = file.read(&mut buffer).map_err(StagingRefusal::store)?;
                        if read == 0 {
                            break;
                        }
                        read_total = read_total.saturating_add(read as u64);
                        if read_total > opened.size {
                            break;
                        }
                        hasher.update(&buffer[..read]);
                    }
                    if read_total != opened.size {
                        return Err(invalid(format!("{relative} changed while it was hashed")));
                    }
                    files.push(TreeFile {
                        path: relative,
                        hash: format!("{:x}", hasher.finalize()),
                        size: opened.size,
                        mode: opened.mode,
                    });
                }
                lillux::TreeEntryKind::Other => {
                    return Err(invalid(format!(
                        "{relative} is a link or special file, which a bundle may not hold"
                    )));
                }
            }
        }
        if entries == 0 {
            return Err(invalid(format!("{} is an empty directory", dir.display())));
        }
    }
    Ok(TreeSummary {
        digest: tree_digest(bundle_name, &files)?,
        file_count: files.len() as u64,
        total_bytes,
    })
}

/// A committed stage with a verified record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedBundle {
    pub record: StagingRecord,
    /// The stage directory.
    pub dir: PathBuf,
}

impl StagedBundle {
    /// The verified tree's directory.
    pub fn tree(&self) -> PathBuf {
        self.dir.join(TREE_DIR)
    }

    /// Check every evidence file against the digest the signed record holds.
    pub fn verify_evidence(&self) -> Result<(), StagingRefusal> {
        for (name, digest) in &self.record.evidence {
            let path = self.dir.join(name);
            let file = std::fs::File::open(&path)
                .map_err(|e| StagingRefusal::Invalid(format!("evidence {name}: {e}")))?;
            let bytes = lillux::read_open_regular_file_bounded(file, MAX_EVIDENCE_BYTES)
                .map_err(|e| StagingRefusal::Invalid(format!("evidence {name}: {e:#}")))?;
            if lillux::sha256_hex(&bytes) != *digest {
                return Err(StagingRefusal::Invalid(format!(
                    "evidence {name} does not match the digest the staging record signs"
                )));
            }
        }
        Ok(())
    }
}

/// A stage under construction. Dropping it removes its directory, so a refused
/// or failed stage leaves nothing behind.
#[derive(Debug)]
pub struct TempStage {
    dir: PathBuf,
    armed: bool,
}

impl TempStage {
    /// The directory the bundle files are materialised into.
    pub fn tree(&self) -> PathBuf {
        self.dir.join(TREE_DIR)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

impl Drop for TempStage {
    fn drop(&mut self) {
        if self.armed
            && let Err(error) = lillux::remove_dir_all_durable(&self.dir)
        {
            tracing::warn!(
                path = %self.dir.display(),
                error = %format!("{error:#}"),
                "failed to remove an abandoned bundle stage directory"
            );
        }
    }
}

/// The result of a commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Committed {
    pub bundle: StagedBundle,
    /// The same bytes were already staged; nothing new was written and no cap
    /// was charged again.
    pub already_staged: bool,
}

/// The staging area of one node.
pub struct StagingArea {
    root: PathBuf,
    directory: lillux::PinnedDirectory,
}

impl StagingArea {
    /// `<state>/bundle-staging` for the node at `app_root`.
    pub fn root_for(app_root: &Path) -> PathBuf {
        ryeos_engine::roots::RuntimeRoot::new(app_root.to_path_buf())
            .state()
            .join(STAGING_DIR)
    }

    /// Open the area, creating it if it is not there.
    pub fn open(app_root: &Path) -> Result<Self, StagingRefusal> {
        let root = Self::root_for(app_root);
        std::fs::create_dir_all(&root).map_err(StagingRefusal::store)?;
        let directory =
            lillux::PinnedDirectory::open_or_create(&root).map_err(StagingRefusal::store)?;
        Ok(Self { root, directory })
    }

    /// Open the area only if it already exists. A read must not create state:
    /// a node that has never staged anything has no area, and `None` says so.
    pub fn open_if_present(app_root: &Path) -> Result<Option<Self>, StagingRefusal> {
        let root = Self::root_for(app_root);
        match std::fs::symlink_metadata(&root) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(StagingRefusal::store(error)),
        }
        let directory =
            lillux::PinnedDirectory::open_or_create(&root).map_err(StagingRefusal::store)?;
        Ok(Some(Self { root, directory }))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Begin a stage: a fresh temporary directory with an empty tree inside.
    pub fn begin(&self) -> Result<TempStage, StagingRefusal> {
        let name = format!("{TEMP_PREFIX}{}", hex::encode(rand::random::<[u8; 16]>()));
        let dir = self.root.join(name);
        std::fs::create_dir(&dir).map_err(StagingRefusal::store)?;
        let stage = TempStage { dir, armed: true };
        std::fs::create_dir(stage.tree()).map_err(StagingRefusal::store)?;
        Ok(stage)
    }

    /// Every committed stage whose record is signed by `node_key`. A directory
    /// with no record, or with a record that does not verify, is not listed.
    pub fn list(&self, node_key: &VerifyingKey) -> Result<Vec<StagedBundle>, StagingRefusal> {
        let mut found = Vec::new();
        for entry in std::fs::read_dir(&self.root).map_err(StagingRefusal::store)? {
            let entry = entry.map_err(StagingRefusal::store)?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if !lillux::valid_hash(&name) {
                continue;
            }
            match self.read_record(&name, node_key) {
                Ok(stage) => found.push(stage),
                Err(error) => {
                    tracing::warn!(stage = %name, error = %error, "ignoring an unreadable bundle stage");
                }
            }
        }
        found.sort_by(|a, b| a.record.tree_digest.cmp(&b.record.tree_digest));
        Ok(found)
    }

    /// Read one stage by digest: the record must be signed by this node's key,
    /// name this digest, and every evidence file must match what it signs.
    pub fn read(
        &self,
        tree_digest: &str,
        node_key: &VerifyingKey,
    ) -> Result<StagedBundle, StagingRefusal> {
        let stage = self.read_record(tree_digest, node_key)?;
        stage.verify_evidence()?;
        Ok(stage)
    }

    fn read_record(
        &self,
        tree_digest: &str,
        node_key: &VerifyingKey,
    ) -> Result<StagedBundle, StagingRefusal> {
        if !lillux::valid_hash(tree_digest) {
            return Err(StagingRefusal::Invalid(
                "a stage is named by a SHA-256 digest".into(),
            ));
        }
        let dir = self.root.join(tree_digest);
        let path = dir.join(RECORD_FILE);
        let file = std::fs::File::open(&path).map_err(|e| {
            StagingRefusal::Invalid(format!("no staging record at {}: {e}", path.display()))
        })?;
        let bytes =
            lillux::read_open_regular_file_bounded(file, MAX_RECORD_BYTES).map_err(|e| {
                StagingRefusal::Invalid(format!("staging record {}: {e:#}", path.display()))
            })?;
        let body =
            ryeos_engine::trust::verify_node_signed_text(&path, &bytes, node_key).map_err(|e| {
                StagingRefusal::Invalid(format!("staging record {}: {e}", path.display()))
            })?;
        let record: StagingRecord = serde_json::from_str(&body).map_err(|e| {
            StagingRefusal::Invalid(format!("staging record {}: {e}", path.display()))
        })?;
        record.validate()?;
        if record.tree_digest != tree_digest {
            return Err(StagingRefusal::Invalid(format!(
                "staging record {} names another stage",
                path.display()
            )));
        }
        Ok(StagedBundle { record, dir })
    }

    /// Refuse now when a stage of `additional_bytes` for `requester` would pass
    /// a cap. Advisory: the commit checks again under the area's lock.
    pub fn check_admission(
        &self,
        caps: &StagingCaps,
        requester: &str,
        additional_bytes: u64,
        node_key: &VerifyingKey,
    ) -> Result<(), StagingRefusal> {
        let held = self.list(node_key)?;
        check_caps(&held, caps, requester, additional_bytes)
    }

    /// Commit a stage: evidence files are written beside the tree, the
    /// temporary directory is renamed to the stage's digest, and the signed
    /// record is written last. `record.evidence` is filled from `evidence`.
    ///
    /// The caps are checked again under the area's exclusive lock. On any
    /// refusal the temporary directory is removed and no record exists.
    pub fn commit(
        &self,
        mut temp: TempStage,
        mut record: StagingRecord,
        evidence: &[(String, Vec<u8>)],
        caps: &StagingCaps,
        node_key: &SigningKey,
    ) -> Result<Committed, StagingRefusal> {
        record.evidence = evidence
            .iter()
            .map(|(name, bytes)| (name.clone(), lillux::sha256_hex(bytes)))
            .collect();
        if record.evidence.len() != evidence.len() {
            return Err(StagingRefusal::Invalid(
                "an evidence file is named twice".into(),
            ));
        }
        record.validate()?;
        let verifying = node_key.verifying_key();

        let _lock = self
            .directory
            .lock_exclusive()
            .map_err(StagingRefusal::store)?;
        self.directory
            .ensure_path_binding()
            .map_err(StagingRefusal::store)?;

        let held = self.list(&verifying)?;
        if let Some(existing) = held
            .iter()
            .find(|stage| stage.record.tree_digest == record.tree_digest)
        {
            return Ok(Committed {
                bundle: existing.clone(),
                already_staged: true,
            });
        }
        check_caps(&held, caps, &record.requested_by, record.total_bytes)?;
        self.sweep(&held);

        let final_dir = self.root.join(&record.tree_digest);
        if final_dir.exists() {
            // Only a crash leaves a digest directory with no record, and the
            // lock is held, so no other commit is between rename and record.
            lillux::remove_dir_all_durable(&final_dir).map_err(StagingRefusal::store)?;
        }
        for (name, bytes) in evidence {
            lillux::atomic_write_private(&temp.dir.join(name), bytes)
                .map_err(StagingRefusal::store)?;
        }
        lillux::sync_tree_durable(&temp.dir).map_err(StagingRefusal::store)?;
        lillux::rename_path_durable(&temp.dir, &final_dir).map_err(StagingRefusal::store)?;
        temp.armed = false;

        let body = serde_json::to_string(&record).map_err(StagingRefusal::store)?;
        let signed = lillux::signature::sign_content(&body, node_key, "#", None);
        if let Err(error) =
            lillux::atomic_write_private(&final_dir.join(RECORD_FILE), signed.as_bytes())
        {
            let _ = lillux::remove_dir_all_durable(&final_dir);
            return Err(StagingRefusal::store(error));
        }
        Ok(Committed {
            bundle: StagedBundle {
                record,
                dir: final_dir,
            },
            already_staged: false,
        })
    }

    /// Remove crash remnants while the lock is held: digest directories with
    /// no verified record and temporary directories a day old. Bounded and
    /// best effort; a remnant that stays is merely not listed.
    fn sweep(&self, held: &[StagedBundle]) {
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return;
        };
        let now = SystemTime::now();
        let mut removed = 0;
        for entry in entries.flatten() {
            if removed >= SWEEP_REMOVE_LIMIT {
                break;
            }
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let remnant = if name.starts_with(TEMP_PREFIX) {
                entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|modified| now.duration_since(modified).ok())
                    .is_some_and(|age| age >= STALE_TEMP_AGE)
            } else {
                lillux::valid_hash(&name)
                    && !held.iter().any(|stage| stage.record.tree_digest == name)
            };
            if remnant && lillux::remove_dir_all_durable(&entry.path()).is_ok() {
                removed += 1;
            }
        }
    }
}

fn check_caps(
    held: &[StagedBundle],
    caps: &StagingCaps,
    requester: &str,
    additional_bytes: u64,
) -> Result<(), StagingRefusal> {
    let node_bytes: u64 = held.iter().map(|s| s.record.total_bytes).sum();
    let key_held: Vec<&StagedBundle> = held
        .iter()
        .filter(|s| s.record.requested_by == requester)
        .collect();
    let key_bytes: u64 = key_held.iter().map(|s| s.record.total_bytes).sum();

    if held.len() >= caps.max_node_stages {
        return Err(StagingRefusal::NodeCount {
            limit: caps.max_node_stages,
        });
    }
    if node_bytes.saturating_add(additional_bytes) > caps.max_node_bytes {
        return Err(StagingRefusal::NodeBytes {
            limit: caps.max_node_bytes,
            held: node_bytes,
            requested: additional_bytes,
        });
    }
    if key_held.len() >= caps.max_key_stages {
        return Err(StagingRefusal::KeyCount {
            limit: caps.max_key_stages,
        });
    }
    if key_bytes.saturating_add(additional_bytes) > caps.max_key_bytes {
        return Err(StagingRefusal::KeyBytes {
            limit: caps.max_key_bytes,
            held: key_bytes,
            requested: additional_bytes,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    fn record(name: &str, requester: &str, bytes: u64) -> StagingRecord {
        let digest = tree_digest(
            name,
            &[TreeFile {
                path: format!("{requester}-{bytes}.txt"),
                hash: lillux::sha256_hex(requester.as_bytes()),
                size: bytes,
                mode: 0o644,
            }],
        )
        .unwrap();
        StagingRecord {
            schema: STAGING_SCHEMA.to_owned(),
            bundle_name: name.to_owned(),
            tree_digest: digest,
            source: StagedSource {
                kind: SOURCE_REMOTE.to_owned(),
                origin: "remote:test".to_owned(),
                publication_record_hash: None,
                pinned_node: None,
                manifest_hash: None,
            },
            requested_by: requester.to_owned(),
            policy_generation: "4".repeat(64),
            declared: DeclaredFacets::default(),
            file_count: 1,
            total_bytes: bytes,
            evidence: BTreeMap::new(),
            staged_at: "2026-11-27T00:00:00Z".to_owned(),
        }
    }

    fn stage(
        area: &StagingArea,
        node: &SigningKey,
        caps: &StagingCaps,
        rec: StagingRecord,
    ) -> Result<Committed, StagingRefusal> {
        let temp = area.begin().unwrap();
        std::fs::write(temp.tree().join("f.txt"), b"x").unwrap();
        area.commit(
            temp,
            rec,
            &[("note.json".to_owned(), b"{}".to_vec())],
            caps,
            node,
        )
    }

    fn entries(area: &StagingArea) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(area.root())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn default_caps_are_the_documented_values() {
        let caps = StagingCaps::default();
        assert_eq!(caps.max_node_bytes, 2 * 1024 * 1024 * 1024);
        assert_eq!(caps.max_node_stages, 64);
        assert_eq!(caps.max_key_bytes, 512 * 1024 * 1024);
        assert_eq!(caps.max_key_stages, 16);
        assert!(caps.max_key_bytes <= caps.max_node_bytes);
        assert!(caps.max_key_stages <= caps.max_node_stages);
    }

    #[test]
    fn a_committed_stage_reads_back_signed_and_with_its_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let area = StagingArea::open(dir.path()).unwrap();
        let node = key(1);
        let committed = stage(
            &area,
            &node,
            &StagingCaps::DEFAULTS,
            record("demo", "aa", 10),
        )
        .unwrap();
        assert!(!committed.already_staged);
        let read = area
            .read(&committed.bundle.record.tree_digest, &node.verifying_key())
            .unwrap();
        assert_eq!(read.record, committed.bundle.record);
        assert!(read.tree().join("f.txt").is_file());
        assert_eq!(read.record.evidence.len(), 1);
        assert_eq!(area.list(&node.verifying_key()).unwrap().len(), 1);
    }

    #[test]
    fn the_same_bytes_staged_twice_are_one_stage() {
        let dir = tempfile::tempdir().unwrap();
        let area = StagingArea::open(dir.path()).unwrap();
        let node = key(1);
        stage(
            &area,
            &node,
            &StagingCaps::DEFAULTS,
            record("demo", "aa", 10),
        )
        .unwrap();
        let again = stage(
            &area,
            &node,
            &StagingCaps::DEFAULTS,
            record("demo", "aa", 10),
        )
        .unwrap();
        assert!(again.already_staged);
        assert_eq!(area.list(&node.verifying_key()).unwrap().len(), 1);
        assert_eq!(
            entries(&area).len(),
            1,
            "the repeat left a temporary directory"
        );
    }

    #[test]
    fn a_record_signed_by_another_key_or_edited_is_refused_when_read() {
        let dir = tempfile::tempdir().unwrap();
        let area = StagingArea::open(dir.path()).unwrap();
        let node = key(1);
        let committed = stage(
            &area,
            &node,
            &StagingCaps::DEFAULTS,
            record("demo", "aa", 10),
        )
        .unwrap();
        let digest = committed.bundle.record.tree_digest.clone();

        // Another node's key does not verify it.
        assert!(area.read(&digest, &key(2).verifying_key()).is_err());
        assert!(area.list(&key(2).verifying_key()).unwrap().is_empty());

        // An edit to the body breaks the signed content hash.
        let path = committed.bundle.dir.join(RECORD_FILE);
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, text.replace("\"demo\"", "\"other\"")).unwrap();
        assert!(area.read(&digest, &node.verifying_key()).is_err());
        assert!(area.list(&node.verifying_key()).unwrap().is_empty());
    }

    #[test]
    fn edited_evidence_is_refused_when_read() {
        let dir = tempfile::tempdir().unwrap();
        let area = StagingArea::open(dir.path()).unwrap();
        let node = key(1);
        let committed = stage(
            &area,
            &node,
            &StagingCaps::DEFAULTS,
            record("demo", "aa", 10),
        )
        .unwrap();
        std::fs::write(committed.bundle.dir.join("note.json"), b"{\"x\":1}").unwrap();
        assert!(
            area.read(&committed.bundle.record.tree_digest, &node.verifying_key())
                .is_err()
        );
    }

    #[test]
    fn a_tree_with_no_record_is_not_listed_and_is_cleaned_by_the_next_commit() {
        let dir = tempfile::tempdir().unwrap();
        let area = StagingArea::open(dir.path()).unwrap();
        let node = key(1);
        // A crash between the rename and the record write.
        let orphan = area.root().join("c".repeat(64));
        std::fs::create_dir_all(orphan.join(TREE_DIR)).unwrap();
        std::fs::write(orphan.join(TREE_DIR).join("f.txt"), b"x").unwrap();
        assert!(area.list(&node.verifying_key()).unwrap().is_empty());
        assert!(area.read(&"c".repeat(64), &node.verifying_key()).is_err());

        stage(
            &area,
            &node,
            &StagingCaps::DEFAULTS,
            record("demo", "aa", 10),
        )
        .unwrap();
        assert!(!orphan.exists(), "the orphan tree was not cleaned");
        assert_eq!(area.list(&node.verifying_key()).unwrap().len(), 1);
    }

    #[test]
    fn caps_refuse_with_typed_codes_and_leave_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let area = StagingArea::open(dir.path()).unwrap();
        let node = key(1);
        let caps = StagingCaps {
            max_node_bytes: 100,
            max_node_stages: 3,
            max_key_bytes: 60,
            max_key_stages: 2,
        };
        stage(&area, &node, &caps, record("one", "aa", 30)).unwrap();
        let before = entries(&area);

        // Key bytes: aa holds 30, +40 passes 60.
        let refused = stage(&area, &node, &caps, record("two", "aa", 40)).unwrap_err();
        assert_eq!(refused.code(), "bundle_stage_key_bytes_cap");
        assert_eq!(refused.status(), 429);
        assert_eq!(entries(&area), before, "a refused stage left something");

        // Node bytes: bb is under its own cap but the node would pass 100.
        stage(&area, &node, &caps, record("three", "bb", 55)).unwrap();
        let refused = stage(&area, &node, &caps, record("four", "cc", 40)).unwrap_err();
        assert_eq!(refused.code(), "bundle_stage_node_bytes_cap");
        assert_eq!(refused.status(), 507);

        // Node count: three stages are allowed; the third fits, a fourth does not.
        stage(&area, &node, &caps, record("five", "dd", 1)).unwrap();
        let refused = stage(&area, &node, &caps, record("six", "ee", 1)).unwrap_err();
        assert_eq!(refused.code(), "bundle_stage_node_count_cap");

        // Nothing was cut: every earlier stage still verifies whole.
        let held = area.list(&node.verifying_key()).unwrap();
        assert_eq!(held.len(), 3);
        for stage in held {
            area.read(&stage.record.tree_digest, &node.verifying_key())
                .unwrap();
            assert!(stage.tree().join("f.txt").is_file());
        }
        assert!(
            entries(&area)
                .iter()
                .all(|name| !name.starts_with(TEMP_PREFIX)),
            "a refusal left a temporary directory"
        );
    }

    #[test]
    fn a_key_over_its_count_cap_is_refused_while_another_key_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let area = StagingArea::open(dir.path()).unwrap();
        let node = key(1);
        let caps = StagingCaps {
            max_key_stages: 2,
            ..StagingCaps::DEFAULTS
        };
        stage(&area, &node, &caps, record("one", "aa", 1)).unwrap();
        stage(&area, &node, &caps, record("two", "aa", 2)).unwrap();
        let refused = stage(&area, &node, &caps, record("three", "aa", 3)).unwrap_err();
        assert_eq!(refused.code(), "bundle_stage_key_count_cap");
        assert_eq!(refused.status(), 429);
        stage(&area, &node, &caps, record("three", "bb", 3)).unwrap();
        assert_eq!(area.list(&node.verifying_key()).unwrap().len(), 3);
    }

    #[test]
    fn an_advisory_check_refuses_before_any_transfer() {
        let dir = tempfile::tempdir().unwrap();
        let area = StagingArea::open(dir.path()).unwrap();
        let node = key(1);
        let caps = StagingCaps {
            max_node_bytes: 10,
            ..StagingCaps::DEFAULTS
        };
        let refused = area
            .check_admission(&caps, "aa", 11, &node.verifying_key())
            .unwrap_err();
        assert_eq!(refused.code(), "bundle_stage_node_bytes_cap");
        area.check_admission(&caps, "aa", 10, &node.verifying_key())
            .unwrap();
    }

    #[test]
    fn the_tree_digest_names_the_bundle_and_ignores_file_order() {
        let a = TreeFile {
            path: "a".into(),
            hash: "1".repeat(64),
            size: 1,
            mode: 0o644,
        };
        let b = TreeFile {
            path: "b".into(),
            hash: "2".repeat(64),
            size: 2,
            mode: 0o755,
        };
        let one = tree_digest("demo", &[a.clone(), b.clone()]).unwrap();
        assert_eq!(one, tree_digest("demo", &[b.clone(), a.clone()]).unwrap());
        assert_ne!(one, tree_digest("other", &[a, b]).unwrap());
    }

    #[test]
    fn a_read_does_not_create_the_staging_area() {
        let dir = tempfile::tempdir().unwrap();
        assert!(StagingArea::open_if_present(dir.path()).unwrap().is_none());
        assert!(!StagingArea::root_for(dir.path()).exists());
        StagingArea::open(dir.path()).unwrap();
        assert!(StagingArea::open_if_present(dir.path()).unwrap().is_some());
    }

    fn tree_with(files: &[(&str, &[u8])]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let tree = dir.path().join("tree");
        for (path, bytes) in files {
            let full = tree.join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(&full, bytes).unwrap();
            lillux::tree_entry::test_support::set_mode(&full, 0o644).unwrap();
        }
        set_directory_modes(&tree);
        (dir, tree)
    }

    /// Give `dir` and every directory below it mode 0o755, whatever the
    /// process umask was when they were created.
    fn set_directory_modes(dir: &Path) {
        lillux::tree_entry::test_support::set_mode(dir, 0o755).unwrap();
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if std::fs::symlink_metadata(&path).unwrap().is_dir() {
                set_directory_modes(&path);
            }
        }
    }

    const LIMITS: TreeLimits = TreeLimits {
        max_files: 100,
        max_bytes: 1 << 20,
    };

    fn listed(files: &[(&str, &[u8])]) -> Vec<TreeFile> {
        files
            .iter()
            .map(|(path, bytes)| TreeFile {
                path: (*path).to_owned(),
                hash: lillux::sha256_hex(bytes),
                size: bytes.len() as u64,
                mode: 0o644,
            })
            .collect()
    }

    #[test]
    fn the_disk_digest_agrees_with_the_entry_list_digest() {
        let files: &[(&str, &[u8])] = &[("b/two.txt", b"two"), ("a.txt", b"one")];
        let (_dir, tree) = tree_with(files);
        let summary = tree_digest_from_dir("demo", &tree, &LIMITS).unwrap();
        assert_eq!(summary.digest, tree_digest("demo", &listed(files)).unwrap());
        assert_eq!(summary.file_count, 2);
        assert_eq!(summary.total_bytes, 6);
    }

    #[test]
    fn a_changed_byte_or_mode_or_extra_file_changes_the_disk_digest() {
        let files: &[(&str, &[u8])] = &[("a.txt", b"one"), ("b.txt", b"two")];
        let (_dir, tree) = tree_with(files);
        let expected = tree_digest("demo", &listed(files)).unwrap();

        std::fs::write(tree.join("a.txt"), b"onE").unwrap();
        let changed = tree_digest_from_dir("demo", &tree, &LIMITS).unwrap();
        assert_ne!(changed.digest, expected);
        std::fs::write(tree.join("a.txt"), b"one").unwrap();
        assert_eq!(
            tree_digest_from_dir("demo", &tree, &LIMITS).unwrap().digest,
            expected
        );

        lillux::tree_entry::test_support::set_mode(&tree.join("a.txt"), 0o755).unwrap();
        assert_ne!(
            tree_digest_from_dir("demo", &tree, &LIMITS).unwrap().digest,
            expected
        );
        lillux::tree_entry::test_support::set_mode(&tree.join("a.txt"), 0o644).unwrap();

        std::fs::write(tree.join("extra.txt"), b"x").unwrap();
        lillux::tree_entry::test_support::set_mode(&tree.join("extra.txt"), 0o644).unwrap();
        assert_ne!(
            tree_digest_from_dir("demo", &tree, &LIMITS).unwrap().digest,
            expected
        );
    }

    #[test]
    fn the_disk_walk_refuses_links_special_files_empty_and_writable_directories() {
        let refused = |tree: &Path| tree_digest_from_dir("demo", tree, &LIMITS).unwrap_err();

        let (_d, tree) = tree_with(&[("a.txt", b"one")]);
        lillux::tree_entry::test_support::make_symlink(Path::new("a.txt"), &tree.join("link"))
            .unwrap();
        assert_eq!(refused(&tree).code(), "bundle_stage_invalid");

        let (_d, tree) = tree_with(&[("a.txt", b"one")]);
        lillux::tree_entry::test_support::make_fifo(&tree.join("pipe")).unwrap();
        assert_eq!(refused(&tree).code(), "bundle_stage_invalid");

        let (_d, tree) = tree_with(&[("a.txt", b"one")]);
        std::fs::create_dir(tree.join("empty")).unwrap();
        lillux::tree_entry::test_support::set_mode(&tree.join("empty"), 0o755).unwrap();
        assert!(refused(&tree).to_string().contains("empty directory"));

        let (_d, tree) = tree_with(&[("sub/a.txt", b"one")]);
        lillux::tree_entry::test_support::set_mode(&tree.join("sub"), 0o775).unwrap();
        assert!(refused(&tree).to_string().contains("writable"));
        lillux::tree_entry::test_support::set_mode(&tree.join("sub"), 0o757).unwrap();
        assert!(refused(&tree).to_string().contains("writable"));
        lillux::tree_entry::test_support::set_mode(&tree.join("sub"), 0o755).unwrap();
        tree_digest_from_dir("demo", &tree, &LIMITS).unwrap();
        lillux::tree_entry::test_support::set_mode(&tree, 0o775).unwrap();
        assert!(refused(&tree).to_string().contains("writable"));
        lillux::tree_entry::test_support::set_mode(&tree, 0o755).unwrap();
        tree_digest_from_dir("demo", &tree, &LIMITS).unwrap();

        // An empty tree holds nothing to digest.
        let dir = tempfile::tempdir().unwrap();
        let empty = dir.path().join("tree");
        std::fs::create_dir(&empty).unwrap();
        lillux::tree_entry::test_support::set_mode(&empty, 0o755).unwrap();
        assert!(refused(&empty).to_string().contains("empty directory"));
    }

    #[test]
    fn the_disk_walk_is_bounded() {
        let (_d, tree) = tree_with(&[("a.txt", b"one"), ("b.txt", b"two")]);
        let few = TreeLimits {
            max_files: 1,
            max_bytes: 1 << 20,
        };
        assert!(tree_digest_from_dir("demo", &tree, &few).is_err());
        let small = TreeLimits {
            max_files: 10,
            max_bytes: 5,
        };
        assert!(tree_digest_from_dir("demo", &tree, &small).is_err());
    }

    #[test]
    fn the_staging_area_is_a_named_child_of_the_state_directory_and_outside_the_bundle_roots() {
        let app = Path::new("/app-root");
        let root = StagingArea::root_for(app);
        let state = ryeos_engine::roots::RuntimeRoot::new(app.to_path_buf()).state();
        assert_eq!(root, state.join("bundle-staging"));
        assert!(!root.starts_with(app.join(ryeos_engine::AI_DIR).join("bundles")));
        assert!(!root.starts_with(app.join(ryeos_engine::AI_DIR).join("node")));
    }

    #[test]
    fn a_node_with_no_staging_area_opens_one_and_lists_nothing() {
        // The doctor rows and state readers name the state children they read;
        // none enumerates the state directory, so this directory is not seen
        // by them. A fresh node simply has none.
        let dir = tempfile::tempdir().unwrap();
        let area = StagingArea::open(dir.path()).unwrap();
        assert!(area.list(&key(1).verifying_key()).unwrap().is_empty());
    }
}
