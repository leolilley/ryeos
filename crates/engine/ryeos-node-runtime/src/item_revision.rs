//! Signed revision history of project items.
//!
//! Every change to a project item (a runtime-authored write, a removal, an
//! operator signature) is one record in that item's chain. A record is a
//! signed item of the core `revision` kind, written into the project beside
//! the item at `.ai/revisions/<kind>/<bare_id>/<seq>.yaml`, so the history
//! travels with the project. The key that made the change signs the record
//! (the node's authoring key for a runtime-authored change, the operator's key
//! for a change the operator made in their own process); the daemon never
//! holds the operator key.
//!
//! A record carries the signed item bytes inline, the digest of the record
//! before it, the digests of the item file before and after, and who acted,
//! all inside the signed body, so the chain verifies from the project alone.
//! A removal is a tombstone: a record whose `after_digest` and `content` are
//! absent.
//!
//! The chain proves what was recorded; it is never authority for what is
//! loaded. The loader reads the file on disk, and a file that differs from the
//! last record reads as an unrecorded change.
//!
//! A project writer could delete the newest records of a chain. The node
//! therefore keeps one node-signed head per project, naming the newest record
//! of each chain it has attested; a chain shorter than, or different from, its
//! head reads as truncated.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

use anyhow::{Context, Result, anyhow, bail, ensure};
use lillux::crypto::{SigningKey, VerifyingKey};
use ryeos_engine::contracts::SignatureEnvelope;
use ryeos_engine::trust::{SignerRole, SignerStatus, TrustStore};
use serde::{Deserialize, Serialize};

use crate::identity::NodeIdentity;

/// The kind of a revision record.
pub const REVISION_KIND: &str = "revision";
/// The directory the `revision` kind schema declares under `.ai`.
pub const REVISIONS_DIRECTORY: &str = "revisions";
/// Schema string every record body carries.
pub const REVISION_SCHEMA: &str = "ryeos.item_revision.v1";
/// Schema string of the node-signed head document.
pub const HEAD_SCHEMA: &str = "ryeos.item_revision_head.v1";
/// Largest signed item a record carries inline.
pub const MAX_INLINE_CONTENT_BYTES: usize = 1024 * 1024;
/// Largest record file read.
pub const MAX_RECORD_BYTES: u64 = 8 * 1024 * 1024;
/// Largest head document read.
const MAX_HEAD_BYTES: u64 = 2 * 1024 * 1024;
/// Most items one project head attests.
pub const MAX_HEAD_ITEMS: usize = 4096;
/// Most records one verified walk reads.
pub const MAX_WALK_RECORDS: usize = 20_000;
const RECORD_EXT: &str = ".yaml";

/// The role a record's signing key holds in the signer registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActorRole {
    /// The node's authoring key, signing a runtime-authored change.
    Authoring,
    /// The operator's key, signing a change made in the operator's process.
    Operator,
}

impl ActorRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Authoring => "authoring",
            Self::Operator => "operator",
        }
    }

    fn registry_role(self) -> SignerRole {
        match self {
            Self::Authoring => SignerRole::Authoring,
            Self::Operator => SignerRole::Operator,
        }
    }
}

/// Who signed a record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RevisionActor {
    /// `fp:<fingerprint>` of the signing key.
    pub fingerprint: String,
    pub role: ActorRole,
}

/// The signed body of one revision record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RevisionBody {
    pub schema: String,
    /// Canonical ref of the item the chain belongs to.
    pub item_ref: String,
    /// Position in the chain, from 1.
    pub seq: u64,
    /// Digest of the previous record, or the genesis string for `seq` 1.
    pub previous: String,
    /// Digest of the item file this change replaced; absent on a create.
    pub before_digest: Option<String>,
    /// Digest of the item file this change wrote; absent on a tombstone.
    pub after_digest: Option<String>,
    /// File extension of the item, with its leading dot.
    pub file_ext: String,
    /// The signed item bytes; absent on a tombstone.
    pub content: Option<String>,
    /// When the change was recorded.
    pub recorded_at: String,
    pub actor: RevisionActor,
    /// What the acting mechanism says about the change (the thread, run and
    /// capability of a runtime-authored change; the operator action of a
    /// client-side one). The substrate reads none of it.
    pub provenance: BTreeMap<String, String>,
}

impl RevisionBody {
    pub fn is_tombstone(&self) -> bool {
        self.after_digest.is_none()
    }
}

/// The previous-record value of the first record of an item's chain.
pub fn genesis_previous(item_ref: &str) -> String {
    format!("genesis:{item_ref}")
}

/// Digest of an item file's bytes, as `before_digest` and `after_digest` name.
///
/// This hashes the whole file, signature line included. An endorsement names
/// a different digest: the one in the signature header's `content_hash`, taken
/// over the bytes after the signature line. The two are not interchangeable;
/// a revision record carries the signed bytes inline (`content`), so the
/// endorsed digest of a revision's item is recomputed from that content.
pub fn file_digest(bytes: &[u8]) -> String {
    lillux::sha256_hex(bytes)
}

fn record_name(seq: u64) -> String {
    format!("{seq:08}{RECORD_EXT}")
}

fn envelope() -> SignatureEnvelope {
    SignatureEnvelope {
        prefix: "#".to_owned(),
        suffix: None,
        after_shebang: false,
    }
}

/// The ref and path parts of an item a chain belongs to.
#[derive(Debug, Clone)]
pub struct ChainItem {
    pub kind: String,
    pub bare_id: String,
}

impl ChainItem {
    pub fn parse(item_ref: &str) -> Result<Self> {
        let canonical = ryeos_engine::canonical_ref::CanonicalRef::parse(item_ref)
            .with_context(|| format!("invalid item ref `{item_ref}`"))?;
        ensure!(
            canonical.suffix.is_none(),
            "an item ref with a suffix has no revision chain"
        );
        ensure!(
            canonical.kind != REVISION_KIND,
            "revision records have no revision chain of their own"
        );
        Ok(Self {
            kind: canonical.kind,
            bare_id: canonical.bare_id,
        })
    }

    pub fn item_ref(&self) -> String {
        format!("{}:{}", self.kind, self.bare_id)
    }

    /// The directory holding this item's records, relative to `.ai`.
    fn relative_directory(&self) -> PathBuf {
        Path::new(REVISIONS_DIRECTORY)
            .join(&self.kind)
            .join(&self.bare_id)
    }

    fn directory(&self, project_root: &Path) -> PathBuf {
        project_root
            .join(ryeos_engine::AI_DIR)
            .join(self.relative_directory())
    }
}

/// What a recorded change came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recorded {
    pub seq: u64,
    /// Digest of the record just written.
    pub digest: String,
    /// Digest of the record before it; absent for the first.
    pub previous_digest: Option<String>,
}

/// One change to record.
pub struct Change<'a> {
    pub item: &'a ChainItem,
    pub file_ext: &'a str,
    /// The item file as it stood before the change; absent on a create.
    pub before: Option<&'a [u8]>,
    /// The signed item bytes the change wrote; absent on a removal.
    pub after: Option<&'a str>,
    pub provenance: BTreeMap<String, String>,
}

/// The newest record of a chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainTip {
    pub seq: u64,
    pub digest: String,
}

fn list_sequences(directory: &Path) -> Result<Vec<u64>> {
    let reader = match std::fs::read_dir(directory) {
        Ok(reader) => reader,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("list {}", directory.display()));
        }
    };
    let mut sequences = Vec::new();
    for entry in reader {
        let entry = entry.with_context(|| format!("list {}", directory.display()))?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Some(stem) = name.strip_suffix(RECORD_EXT) else {
            continue;
        };
        if stem.len() != 8 || !stem.bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }
        ensure!(
            entry
                .file_type()
                .with_context(|| format!("inspect {}", entry.path().display()))?
                .is_file(),
            "revision record {} is not a regular file",
            entry.path().display()
        );
        sequences.push(stem.parse::<u64>()?);
        ensure!(
            sequences.len() <= MAX_WALK_RECORDS * 2,
            "the revision chain {} holds too many records to list",
            directory.display()
        );
    }
    sequences.sort_unstable();
    Ok(sequences)
}

fn read_record_bytes(directory: &Path, seq: u64) -> Result<Option<Vec<u8>>> {
    lillux::read_optional_regular_file_bounded_no_follow(
        &directory.join(record_name(seq)),
        MAX_RECORD_BYTES,
    )
}

/// The newest record of `item`'s chain. A chain whose records do not run
/// 1..=n without a gap is an error: nothing is appended to a broken chain.
pub fn chain_tip(project_root: &Path, item: &ChainItem) -> Result<Option<ChainTip>> {
    let directory = item.directory(project_root);
    let sequences = list_sequences(&directory)?;
    let Some(last) = sequences.last().copied() else {
        return Ok(None);
    };
    ensure!(
        sequences.len() as u64 == last && sequences.first() == Some(&1),
        "the revision chain of `{}` is not contiguous (records 1..={last} expected, {} present)",
        item.item_ref(),
        sequences.len()
    );
    let bytes = read_record_bytes(&directory, last)?
        .ok_or_else(|| anyhow!("revision record {last} of `{}` vanished", item.item_ref()))?;
    Ok(Some(ChainTip {
        seq: last,
        digest: file_digest(&bytes),
    }))
}

fn open_chain_directory(project_root: &Path, item: &ChainItem) -> Result<lillux::PinnedDirectory> {
    let project = lillux::PinnedDirectory::open(project_root)?.ok_or_else(|| {
        anyhow!(
            "project root disappeared while recording a revision: {}",
            project_root.display()
        )
    })?;
    let mut current = project
        .open_child_directory(OsStr::new(ryeos_engine::AI_DIR))?
        .ok_or_else(|| anyhow!("project has no {} directory", ryeos_engine::AI_DIR))?;
    for component in item.relative_directory().components() {
        let std::path::Component::Normal(segment) = component else {
            bail!(
                "revision chain path of `{}` has a non-relative component",
                item.item_ref()
            );
        };
        current = current
            .open_or_create_child(segment, 0o755)
            .with_context(|| {
                format!("open or create the revision chain of `{}`", item.item_ref())
            })?;
    }
    Ok(current)
}

/// Append one record to `change.item`'s chain, signed by `signer`.
///
/// The caller holds whatever lock serializes writers of the item; a record
/// that would take a sequence another writer already took is refused rather
/// than overwritten.
pub fn append(
    project_root: &Path,
    signer: &SigningKey,
    role: ActorRole,
    change: Change<'_>,
) -> Result<Recorded> {
    let item_ref = change.item.item_ref();
    if let Some(after) = change.after {
        ensure!(
            after.len() <= MAX_INLINE_CONTENT_BYTES,
            "`{item_ref}` is {} bytes; a revision record carries at most {MAX_INLINE_CONTENT_BYTES}",
            after.len()
        );
    }
    let tip = chain_tip(project_root, change.item)?;
    let (seq, previous, previous_digest) = match &tip {
        Some(tip) => (tip.seq + 1, tip.digest.clone(), Some(tip.digest.clone())),
        None => (1, genesis_previous(&item_ref), None),
    };
    let body = RevisionBody {
        schema: REVISION_SCHEMA.to_owned(),
        item_ref: item_ref.clone(),
        seq,
        previous,
        before_digest: change.before.map(file_digest),
        after_digest: change.after.map(|after| file_digest(after.as_bytes())),
        file_ext: change.file_ext.to_owned(),
        content: change.after.map(str::to_owned),
        recorded_at: lillux::time::iso8601_now(),
        actor: RevisionActor {
            fingerprint: format!(
                "fp:{}",
                lillux::signature::compute_fingerprint(&signer.verifying_key())
            ),
            role,
        },
        provenance: change.provenance,
    };
    let yaml = serde_yaml::to_string(&body).context("render the revision record")?;
    let signed = lillux::signature::sign_content(&yaml, signer, "#", None);
    ensure!(
        signed.len() as u64 <= MAX_RECORD_BYTES,
        "the revision record of `{item_ref}` exceeds {MAX_RECORD_BYTES} bytes"
    );
    let directory = open_chain_directory(project_root, change.item)?;
    let name = OsString::from(record_name(seq));
    directory.ensure_path_binding()?;
    let result = directory.replace_bytes_if_matches_atomic(
        &name,
        None,
        |_| Ok(()),
        signed.as_bytes(),
        0o644,
    );
    match result {
        Ok(()) => {}
        Err(error) if error.namespace_committed() => {
            directory.sync().map_err(|sync_error| {
                anyhow!(
                    "revision record {seq} of `{item_ref}` was committed, but its durability \
                     remains uncertain: {error}; {sync_error:#}"
                )
            })?;
        }
        Err(error) => {
            return Err(anyhow!(error))
                .with_context(|| format!("write revision record {seq} of `{item_ref}`"));
        }
    }
    Ok(Recorded {
        seq,
        digest: file_digest(signed.as_bytes()),
        previous_digest,
    })
}

// ── verification ────────────────────────────────────────────────────────

/// Why a record did not verify.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureCode {
    Missing,
    Unreadable,
    Unparseable,
    SignatureInvalid,
    SignerNotPinned,
    SignerRevoked,
    SignerRoleNotPermitted,
    ActorMismatch,
    BodyMismatch,
    PreviousMismatch,
    PreviousUnverifiable,
    ContentMismatch,
}

/// The outcome of verifying one record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RecordVerify {
    pub status: &'static str,
    pub reason: Option<FailureCode>,
    pub detail: Option<String>,
    pub signer: Option<String>,
}

impl RecordVerify {
    fn verified(signer: String) -> Self {
        Self {
            status: "verified",
            reason: None,
            detail: None,
            signer: Some(signer),
        }
    }

    fn failed(reason: FailureCode, detail: impl Into<String>, signer: Option<String>) -> Self {
        Self {
            status: "failed",
            reason: Some(reason),
            detail: Some(detail.into()),
            signer,
        }
    }

    pub fn is_verified(&self) -> bool {
        self.status == "verified"
    }
}

/// One record of a walked chain.
#[derive(Debug, Clone)]
pub struct WalkedRecord {
    pub seq: u64,
    /// Digest of the record file, absent when the file is missing.
    pub digest: Option<String>,
    pub body: Option<RevisionBody>,
    pub verify: RecordVerify,
    /// The record's `before_digest` is not the previous record's
    /// `after_digest`: the item changed between them by some other means.
    pub unrecorded_change_before: bool,
}

/// Verify the signature and signer of one record's bytes.
fn verify_signature_and_signer(
    content: &str,
    trust: &TrustStore,
) -> std::result::Result<(String, SignerRole), (FailureCode, String, Option<String>)> {
    let envelope = envelope();
    let header = ryeos_engine::item_resolution::parse_signature_header(content, &envelope)
        .ok_or_else(|| {
            (
                FailureCode::SignatureInvalid,
                "the record has no signature line".to_owned(),
                None,
            )
        })?;
    let signer = format!("fp:{}", header.signer_fingerprint);
    let fail = |code, detail: &str| Err((code, detail.to_owned(), Some(signer.clone())));
    let actual =
        ryeos_engine::trust::content_hash_after_signature(content, &envelope).ok_or_else(|| {
            (
                FailureCode::SignatureInvalid,
                "the record's signature line cannot be located".to_owned(),
                Some(signer.clone()),
            )
        })?;
    if actual != header.content_hash {
        return fail(
            FailureCode::SignatureInvalid,
            "the record's bytes do not match the hash its signature covers",
        );
    }
    let Some(record) = trust.signer(&header.signer_fingerprint) else {
        return fail(
            FailureCode::SignerNotPinned,
            "the signing key is not pinned in this node's signer registry",
        );
    };
    if record.status == SignerStatus::Revoked {
        return fail(FailureCode::SignerRevoked, "the signing key is revoked");
    }
    let Some(trusted) = trust.get(&header.signer_fingerprint) else {
        return fail(
            FailureCode::SignerNotPinned,
            "the signing key is not trusted by this node",
        );
    };
    if !lillux::signature::verify_signature(
        &header.content_hash,
        &header.signature_b64,
        &trusted.verifying_key,
    ) {
        return fail(
            FailureCode::SignatureInvalid,
            "the signature does not verify under the signing key",
        );
    }
    if !matches!(record.role, SignerRole::Authoring | SignerRole::Operator) {
        return fail(
            FailureCode::SignerRoleNotPermitted,
            "only an authoring or operator key may sign a revision record",
        );
    }
    Ok((signer, record.role))
}

fn verify_record_bytes(
    bytes: &[u8],
    item: &ChainItem,
    seq: u64,
    trust: &TrustStore,
) -> (Option<RevisionBody>, RecordVerify) {
    let Ok(content) = std::str::from_utf8(bytes) else {
        return (
            None,
            RecordVerify::failed(FailureCode::Unparseable, "the record is not UTF-8", None),
        );
    };
    let (signer, role) = match verify_signature_and_signer(content, trust) {
        Ok(verified) => verified,
        Err((code, detail, signer)) => {
            let body = parse_body(content).ok();
            return (body, RecordVerify::failed(code, detail, signer));
        }
    };
    let body = match parse_body(content) {
        Ok(body) => body,
        Err(error) => {
            return (
                None,
                RecordVerify::failed(FailureCode::Unparseable, format!("{error:#}"), Some(signer)),
            );
        }
    };
    let failure =
        if body.schema != REVISION_SCHEMA || body.item_ref != item.item_ref() || body.seq != seq {
            Some((
                FailureCode::BodyMismatch,
                "the record does not name this item and position".to_owned(),
            ))
        } else if body.actor.fingerprint != signer || role != body.actor.role.registry_role() {
            Some((
                FailureCode::ActorMismatch,
                "the record's actor is not the key and role that signed it".to_owned(),
            ))
        } else if body.content.as_deref().map(|c| file_digest(c.as_bytes())) != body.after_digest {
            Some((
                FailureCode::ContentMismatch,
                "the record's inline content is not what its after_digest names".to_owned(),
            ))
        } else {
            None
        };
    match failure {
        Some((code, detail)) => (Some(body), RecordVerify::failed(code, detail, Some(signer))),
        None => (Some(body), RecordVerify::verified(signer)),
    }
}

/// The body a signature line covers: everything after the first line. Only
/// that line is the record's own signature; a signed item carried inline in the
/// body has a signature line of its own that must stay.
fn signed_body(content: &str) -> Result<&str> {
    let (first, body) = content
        .split_once('\n')
        .ok_or_else(|| anyhow!("the document has no body after its signature line"))?;
    ensure!(
        first.starts_with(&format!("# {}", lillux::signature::SIGNATURE_PREFIX)),
        "the document does not start with its signature line"
    );
    Ok(body)
}

fn parse_body(content: &str) -> Result<RevisionBody> {
    serde_yaml::from_str(signed_body(content)?).context("parse the revision record body")
}

/// Walk and verify a chain from record `from` (1 for the whole chain) through
/// its last, in order. `prior` is the digest the first walked record must
/// continue: the genesis string for `from == 1`, otherwise the digest of record
/// `from - 1`.
fn walk_range(
    directory: &Path,
    item: &ChainItem,
    trust: &TrustStore,
    from: u64,
    last: u64,
    prior: Option<String>,
    prior_after_digest: Option<Option<String>>,
) -> Result<Vec<WalkedRecord>> {
    ensure!(
        last.saturating_sub(from) < MAX_WALK_RECORDS as u64,
        "the revision chain of `{}` is too long to verify in one walk",
        item.item_ref()
    );
    let mut records = Vec::new();
    let mut prior = prior;
    let mut prior_after = prior_after_digest;
    for seq in from..=last {
        let bytes = match read_record_bytes(directory, seq) {
            Ok(Some(bytes)) => bytes,
            Ok(None) => {
                records.push(WalkedRecord {
                    seq,
                    digest: None,
                    body: None,
                    verify: RecordVerify::failed(
                        FailureCode::Missing,
                        "the record is missing from the chain",
                        None,
                    ),
                    unrecorded_change_before: false,
                });
                prior = None;
                prior_after = None;
                continue;
            }
            Err(error) => {
                records.push(WalkedRecord {
                    seq,
                    digest: None,
                    body: None,
                    verify: RecordVerify::failed(
                        FailureCode::Unreadable,
                        format!("{error:#}"),
                        None,
                    ),
                    unrecorded_change_before: false,
                });
                prior = None;
                prior_after = None;
                continue;
            }
        };
        let digest = file_digest(&bytes);
        let (body, mut verify) = verify_record_bytes(&bytes, item, seq, trust);
        let mut unrecorded = false;
        if let Some(body) = &body {
            if verify.is_verified() {
                match &prior {
                    Some(prior) if *prior != body.previous => {
                        verify = RecordVerify::failed(
                            FailureCode::PreviousMismatch,
                            "the record does not continue the record before it",
                            verify.signer.clone(),
                        );
                    }
                    None if seq > 1 || from > 1 => {
                        verify = RecordVerify::failed(
                            FailureCode::PreviousUnverifiable,
                            "the record before it is missing or unreadable",
                            verify.signer.clone(),
                        );
                    }
                    _ => {}
                }
            }
            if let Some(prior_after) = &prior_after {
                unrecorded = body.before_digest != *prior_after;
            }
            prior_after = Some(body.after_digest.clone());
        } else {
            prior_after = None;
        }
        prior = Some(digest.clone());
        records.push(WalkedRecord {
            seq,
            digest: Some(digest),
            body,
            verify,
            unrecorded_change_before: unrecorded,
        });
    }
    Ok(records)
}

// ── the node-signed head ────────────────────────────────────────────────

/// What the node attested for one chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeadEntry {
    pub seq: u64,
    pub digest: String,
    pub recorded_at: String,
}

/// The node-signed head document of one project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HeadDocument {
    schema: String,
    project: String,
    node_fingerprint: String,
    items: BTreeMap<String, HeadEntry>,
}

/// The state of a project's attested heads as read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectHead {
    /// No head document exists for the project.
    Absent,
    Present(BTreeMap<String, HeadEntry>),
    /// A document exists and is not the node's valid signed head.
    Invalid(String),
}

/// The node-signed heads of every project the node has recorded for.
pub struct RevisionHeads {
    directory: PathBuf,
    node_key: VerifyingKey,
    signer: Option<SigningKey>,
}

static HEAD_WRITE: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

impl RevisionHeads {
    /// Heads under `app_root`, written with the node's key.
    pub fn for_writer(app_root: &Path, node: &NodeIdentity) -> Self {
        Self {
            directory: ryeos_engine::roots::RuntimeRoot::new(app_root.to_path_buf())
                .revision_heads_dir(),
            node_key: *node.verifying_key(),
            signer: Some(node.signing_key().clone()),
        }
    }

    /// Heads under `app_root`, read and checked against the node's key.
    pub fn for_reader(app_root: &Path, node_key: VerifyingKey) -> Self {
        Self {
            directory: ryeos_engine::roots::RuntimeRoot::new(app_root.to_path_buf())
                .revision_heads_dir(),
            node_key,
            signer: None,
        }
    }

    fn path(&self, project: &str) -> PathBuf {
        self.directory
            .join(format!("{}.yaml", lillux::sha256_hex(project.as_bytes())))
    }

    /// The name a project is attested under: its canonical path, so the
    /// writer and every reader agree however the root was spelled.
    fn project_key(project_root: &Path) -> Result<String> {
        let canonical = std::fs::canonicalize(project_root)
            .with_context(|| format!("resolve project root {}", project_root.display()))?;
        canonical
            .to_str()
            .map(str::to_owned)
            .ok_or_else(|| anyhow!("project root is not valid UTF-8"))
    }

    /// Read and check the project's head document.
    pub fn read(&self, project_root: &Path) -> Result<ProjectHead> {
        let project = Self::project_key(project_root)?;
        let path = self.path(&project);
        let Some(bytes) =
            lillux::read_optional_regular_file_bounded_no_follow(&path, MAX_HEAD_BYTES)?
        else {
            return Ok(ProjectHead::Absent);
        };
        Ok(match self.verify_document(&bytes, &project) {
            Ok(document) => ProjectHead::Present(document.items),
            Err(error) => ProjectHead::Invalid(format!("{error:#}")),
        })
    }

    fn verify_document(&self, bytes: &[u8], project: &str) -> Result<HeadDocument> {
        let content = std::str::from_utf8(bytes).context("the head document is not UTF-8")?;
        let envelope = envelope();
        let header = ryeos_engine::item_resolution::parse_signature_header(content, &envelope)
            .ok_or_else(|| anyhow!("the head document has no signature line"))?;
        ryeos_engine::trust::verify_node_signed_document(
            content,
            &header,
            &envelope,
            &self.node_key,
        )
        .context("the head document is not signed by this node")?;
        let document: HeadDocument =
            serde_yaml::from_str(signed_body(content)?).context("parse the head document")?;
        ensure!(
            document.schema == HEAD_SCHEMA
                && document.project == project
                && document.node_fingerprint
                    == lillux::signature::compute_fingerprint(&self.node_key),
            "the head document is not this node's head for this project"
        );
        Ok(document)
    }

    /// Attest `entry` as the head of `item_ref` in `project_root`.
    pub fn record(
        &self,
        project_root: &Path,
        item_ref: &str,
        seq: u64,
        digest: &str,
    ) -> Result<()> {
        let signer = self
            .signer
            .as_ref()
            .ok_or_else(|| anyhow!("these heads were opened for reading"))?;
        let project = Self::project_key(project_root)?;
        let _guard = HEAD_WRITE.lock().unwrap_or_else(|error| error.into_inner());
        let mut items = match self.read(project_root)? {
            ProjectHead::Absent => BTreeMap::new(),
            ProjectHead::Present(items) => items,
            ProjectHead::Invalid(reason) => {
                bail!("the node's revision head for this project is invalid: {reason}")
            }
        };
        if !items.contains_key(item_ref) {
            ensure!(
                items.len() < MAX_HEAD_ITEMS,
                "the project head already attests {MAX_HEAD_ITEMS} items"
            );
        }
        items.insert(
            item_ref.to_owned(),
            HeadEntry {
                seq,
                digest: digest.to_owned(),
                recorded_at: lillux::time::iso8601_now(),
            },
        );
        let document = HeadDocument {
            schema: HEAD_SCHEMA.to_owned(),
            project: project.clone(),
            node_fingerprint: lillux::signature::compute_fingerprint(&self.node_key),
            items,
        };
        let yaml = serde_yaml::to_string(&document).context("render the head document")?;
        let signed = lillux::signature::sign_content(&yaml, signer, "#", None);
        ensure!(
            signed.len() as u64 <= MAX_HEAD_BYTES,
            "the project head exceeds {MAX_HEAD_BYTES} bytes"
        );
        std::fs::create_dir_all(&self.directory)
            .with_context(|| format!("create {}", self.directory.display()))?;
        lillux::atomic_write_private(&self.path(&project), signed.as_bytes())
            .map_err(anyhow::Error::from)
            .context("write the head document")
    }
}

/// How a chain stands against the node's attested head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeadStatus {
    /// The chain ends at the attested head.
    Attested { seq: u64 },
    /// The chain continues past the attested head, by records not yet
    /// attested (an operator change made while the node was stopped).
    Behind { head_seq: u64, tip_seq: u64 },
    /// The chain ends before the attested head: newest records were removed.
    Truncated { head_seq: u64, tip_seq: u64 },
    /// The record at the attested position is not the attested record.
    Diverged { head_seq: u64 },
    /// The node has attested nothing for this chain.
    Unattested,
    /// The head document cannot be trusted.
    HeadInvalid { reason: String },
}

impl HeadStatus {
    pub fn is_sound(&self) -> bool {
        matches!(
            self,
            Self::Attested { .. } | Self::Behind { .. } | Self::Unattested
        )
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::Attested { .. } => "attested",
            Self::Behind { .. } => "behind",
            Self::Truncated { .. } => "truncated",
            Self::Diverged { .. } => "diverged",
            Self::Unattested => "unattested",
            Self::HeadInvalid { .. } => "head_invalid",
        }
    }
}

fn head_status(head: &ProjectHead, item_ref: &str, records: &[WalkedRecord]) -> HeadStatus {
    let entry = match head {
        ProjectHead::Invalid(reason) => {
            return HeadStatus::HeadInvalid {
                reason: reason.clone(),
            };
        }
        ProjectHead::Absent => return HeadStatus::Unattested,
        ProjectHead::Present(items) => match items.get(item_ref) {
            Some(entry) => entry,
            None => return HeadStatus::Unattested,
        },
    };
    let tip_seq = records.last().map_or(0, |record| record.seq);
    if tip_seq < entry.seq {
        return HeadStatus::Truncated {
            head_seq: entry.seq,
            tip_seq,
        };
    }
    let at_head = records
        .iter()
        .find(|record| record.seq == entry.seq)
        .and_then(|record| record.digest.as_deref());
    if at_head != Some(entry.digest.as_str()) {
        return HeadStatus::Diverged {
            head_seq: entry.seq,
        };
    }
    if tip_seq == entry.seq {
        HeadStatus::Attested { seq: entry.seq }
    } else {
        HeadStatus::Behind {
            head_seq: entry.seq,
            tip_seq,
        }
    }
}

// ── reading a whole chain ───────────────────────────────────────────────

/// How the live item file stands against the chain's last record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FileStatus {
    /// The file is the bytes the last record wrote.
    Matches,
    /// The file is not what the last record wrote, or exists after a
    /// tombstone: it was changed by something that recorded nothing.
    UnrecordedChange,
    /// The file is gone though the last record did not remove it.
    UnrecordedRemoval,
    /// The last record is a tombstone and the file is gone.
    Removed,
    /// The chain has no records.
    NoHistory,
}

/// A verified read of one item's chain.
#[derive(Debug, Clone)]
pub struct ChainView {
    pub item_ref: String,
    pub records: Vec<WalkedRecord>,
    pub head: HeadStatus,
    pub file: FileStatus,
}

impl ChainView {
    /// Every record verified and the head holds.
    pub fn is_sound(&self) -> bool {
        self.head.is_sound()
            && self
                .records
                .iter()
                .all(|record| record.verify.is_verified())
    }
}

/// Read and verify `item`'s whole chain, and judge the live item file against
/// it. `item_directory` is the directory the item's kind keeps its files in.
pub fn read_chain(
    project_root: &Path,
    item_directory: &Path,
    item: &ChainItem,
    trust: &TrustStore,
    heads: &RevisionHeads,
) -> Result<ChainView> {
    let directory = item.directory(project_root);
    let sequences = list_sequences(&directory)?;
    let last = sequences.last().copied().unwrap_or(0);
    let records = if last == 0 {
        Vec::new()
    } else {
        walk_range(
            &directory,
            item,
            trust,
            1,
            last,
            Some(genesis_previous(&item.item_ref())),
            None,
        )?
    };
    let head = head_status(&heads.read(project_root)?, &item.item_ref(), &records);
    let file = match records.last().and_then(|record| record.body.as_ref()) {
        None => FileStatus::NoHistory,
        Some(body) => {
            let path = item_directory.join(format!("{}{}", item.bare_id, body.file_ext));
            let live = lillux::read_optional_regular_file_bounded_no_follow(
                &path,
                ryeos_engine::item_resolution::MAX_ITEM_SOURCE_BYTES,
            )?;
            match (live, &body.after_digest) {
                (None, None) => FileStatus::Removed,
                (None, Some(_)) => FileStatus::UnrecordedRemoval,
                (Some(_), None) => FileStatus::UnrecordedChange,
                (Some(bytes), Some(after)) if file_digest(&bytes) == *after => FileStatus::Matches,
                (Some(_), Some(_)) => FileStatus::UnrecordedChange,
            }
        }
    };
    Ok(ChainView {
        item_ref: item.item_ref(),
        records,
        head,
        file,
    })
}

// ── the writer's pre-append check and post-append attestation ───────────

/// Why the writer refuses to extend a chain.
#[derive(Debug, thiserror::Error)]
pub enum ChainRefusal {
    #[error(
        "the revision chain of `{item_ref}` ends at record {tip_seq}, before the node's attested \
         head at record {head_seq}: newest records were removed from the project"
    )]
    Truncated {
        item_ref: String,
        head_seq: u64,
        tip_seq: u64,
    },
    #[error(
        "the revision chain of `{item_ref}` has a different record at position {head_seq} than \
         the one the node attested"
    )]
    Diverged { item_ref: String, head_seq: u64 },
    #[error("the revision chain of `{item_ref}` does not verify at record {seq}: {detail}")]
    Broken {
        item_ref: String,
        seq: u64,
        detail: String,
    },
    #[error("the node's revision head for this project is invalid: {reason}")]
    HeadInvalid { reason: String },
}

impl ChainRefusal {
    /// Stable code for the callback error envelope.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Truncated { .. } => "revision_chain_truncated",
            Self::Diverged { .. } => "revision_chain_diverged",
            Self::Broken { .. } => "revision_chain_broken",
            Self::HeadInvalid { .. } => "revision_head_invalid",
        }
    }
}

/// Check a chain before extending it: the node's head still holds, and every
/// record past the head (or the whole chain when the node has attested none)
/// verifies. The check reads only the records it must, so a long attested
/// chain costs one record to extend.
pub fn check_before_append(
    project_root: &Path,
    item: &ChainItem,
    trust: &TrustStore,
    heads: &RevisionHeads,
) -> Result<()> {
    let item_ref = item.item_ref();
    let directory = item.directory(project_root);
    let sequences = list_sequences(&directory)?;
    let tip = sequences.last().copied().unwrap_or(0);
    let head = heads.read(project_root)?;
    let attested = match &head {
        ProjectHead::Invalid(reason) => {
            return Err(ChainRefusal::HeadInvalid {
                reason: reason.clone(),
            }
            .into());
        }
        ProjectHead::Absent => None,
        ProjectHead::Present(items) => items.get(&item_ref).cloned(),
    };
    let (from, prior, prior_after) = match attested {
        None => (1, Some(genesis_previous(&item_ref)), None),
        Some(entry) => {
            if tip < entry.seq {
                return Err(ChainRefusal::Truncated {
                    item_ref,
                    head_seq: entry.seq,
                    tip_seq: tip,
                }
                .into());
            }
            let at_head = read_record_bytes(&directory, entry.seq)?;
            if at_head.as_deref().map(file_digest).as_deref() != Some(entry.digest.as_str()) {
                return Err(ChainRefusal::Diverged {
                    item_ref,
                    head_seq: entry.seq,
                }
                .into());
            }
            (entry.seq + 1, Some(entry.digest), None)
        }
    };
    if tip == 0 || from > tip {
        return Ok(());
    }
    let walked = walk_range(&directory, item, trust, from, tip, prior, prior_after)?;
    if let Some(bad) = walked.iter().find(|record| !record.verify.is_verified()) {
        return Err(ChainRefusal::Broken {
            item_ref,
            seq: bad.seq,
            detail: bad
                .verify
                .detail
                .clone()
                .unwrap_or_else(|| "the record failed verification".to_owned()),
        }
        .into());
    }
    Ok(())
}

/// Attest the newest record of a chain as the node's head for it.
pub fn attest(
    heads: &RevisionHeads,
    project_root: &Path,
    item: &ChainItem,
    recorded: &Recorded,
) -> Result<()> {
    heads.record(
        project_root,
        &item.item_ref(),
        recorded.seq,
        &recorded.digest,
    )
}

#[cfg(test)]
mod tests;
