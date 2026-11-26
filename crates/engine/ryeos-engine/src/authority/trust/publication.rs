//! Publications: a node's operator offers one frozen bundle export to anyone
//! who holds a link, for a bounded time.
//!
//! The operator signs a publication record with their own key, client side.
//! The node never holds that key: at publish it checks that the signer is an
//! active operator in its signer registry and that the signature verifies,
//! then countersigns. The node countersignature covers the record body and the
//! digest of the operator signature, so a stored pair cannot be re-paired with
//! another operator signature. The node verified that its active operator
//! signed; a puller verifies the node, against the node key it pinned.
//!
//! The served form carries the body and the node countersignature only. The
//! operator signature stays in the node's stored record: a signature, though it
//! does not name its key, can be tested against a candidate public key, and
//! that would link this node's operator to a key seen elsewhere.
//!
//! The closure a publication may serve is not stored. It is derived in memory
//! from the frozen export manifest, a content-addressed object the record
//! names: the manifest hash plus every entry hash.
//!
//! Retiring a publication writes a node-signed tombstone named by the
//! publication id, which stays in force if the record file is put back. The
//! tombstone records when and why (a category), not which operator request
//! retired it: version 1 does not record that.
//!
//! Layout below `<state>/publications/`:
//!
//! ```text
//! <id>.json            node-signed stored publication
//! retired/<id>.json    node-signed tombstone
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use lillux::crypto::{Signer as _, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};

use super::{
    MAX_TRUST_DOCUMENT_BYTES, MAX_TRUST_DOCUMENTS, MAX_TRUST_TRAVERSAL_ENTRIES, SignerRole,
    SignerStatus, TrustStore, verify_node_signed_text,
};
use crate::roots::RuntimeRoot;

/// Schema tag of the publication record body.
pub const PUBLICATION_SCHEMA: &str = "ryeos.publication/v1";
/// Schema tag of the frozen export manifest.
pub const PUBLICATION_MANIFEST_SCHEMA: &str = "ryeos.publication.bundle-export-manifest/v1";
/// Object kind the frozen manifest carries, so the content store's
/// reachability walk can follow its entries to their blobs.
pub const PUBLICATION_MANIFEST_KIND: &str = "publication_manifest";
/// Root kind of a publication that serves a frozen bundle export.
pub const PUBLICATION_ROOT_BUNDLE_EXPORT: &str = "bundle_export";
/// What every record states about retiring and copying.
pub const PUBLICATION_STATEMENT: &str = "published by the operator of this node; retiring it stops this node serving it; copies already taken are not recalled and remain verifiable against the publisher's signature";
/// What the tombstone and the capability report state about who retired.
pub const PUBLICATION_RETIREMENT_NOTE: &str = "version 1 does not record which operator request retired a publication; the tombstone is node-signed and names only when and why";
/// What the capability report states about node key rotation.
pub const PUBLICATION_KEY_ROTATION_NOTE: &str = "a node key rotation invalidates every printed node pin: existing publication links stop verifying; re-share them";

const OPERATOR_DOMAIN: &str = "ryeos-publication-v1";
const NODE_DOMAIN: &str = "ryeos-publication-node-v1";
const STORE_SCHEMA_VERSION: u32 = 1;
/// Name of the directory below the state directory.
pub const PUBLICATIONS_DIR: &str = "publications";
/// Directory of tombstones inside it.
pub const RETIRED_DIR: &str = "retired";
/// Longest lifetime code allows a publication, whatever node data says.
pub const PUBLICATION_HARD_MAX_LIFETIME_DAYS: u32 = 180;
/// Most publications one node stores, live or retired.
pub const MAX_PUBLICATIONS: usize = 256;
/// How long the capability report lists a retired publication.
pub const RETIRED_REPORT_WINDOW_SECS: u64 = 30 * 24 * 60 * 60;
/// How far a record's creation time may differ from the node's clock.
pub const MAX_PUBLICATION_CLOCK_SKEW_SECS: u64 = 300;
/// Smallest and largest per-request read bound a record may carry.
pub const MIN_PUBLICATION_READ_BYTES: u64 = 4096;
pub const MAX_PUBLICATION_READ_BYTES: u64 = 32 * 1024 * 1024;
/// Bounds on the frozen manifest.
pub const MAX_PUBLICATION_MANIFEST_BYTES: u64 = 8 * 1024 * 1024;
pub const MAX_PUBLICATION_MANIFEST_ENTRIES: usize = 10_000;

/// Why a publication call was refused. Every refusal has a stable code.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PublicationRefusal {
    #[error("publication_malformed: {0}")]
    Malformed(String),
    #[error(
        "publication_bad_signature: the record does not verify under the operator key. Sign it \
         again with the operator key"
    )]
    BadSignature,
    #[error(
        "publication_signer_not_operator: the signer is not an active operator in this node's \
         signer registry"
    )]
    SignerNotOperator,
    #[error("publication_expired: not_after has passed")]
    Expired,
    #[error("publication_not_after_too_far: not_after is more than {max_days} days away")]
    NotAfterTooFar { max_days: u32 },
    #[error("publication_not_found: no such publication")]
    NotFound,
    #[error("publication_retired: this publication was retired")]
    Retired {
        retired_at: String,
        reason: PublicationReason,
    },
    #[error("publication_exists: a publication with this id is already recorded")]
    AlreadyExists,
    #[error("publication_store: {0}")]
    Store(String),
}

impl PublicationRefusal {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Malformed(_) => "publication_malformed",
            Self::BadSignature => "publication_bad_signature",
            Self::SignerNotOperator => "publication_signer_not_operator",
            Self::Expired => "publication_expired",
            Self::NotAfterTooFar { .. } => "publication_not_after_too_far",
            Self::NotFound => "publication_not_found",
            Self::Retired { .. } => "publication_retired",
            Self::AlreadyExists => "publication_exists",
            Self::Store(_) => "publication_store",
        }
    }
}

/// Why a publication stopped being served: a closed category, never free text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicationReason {
    Operator,
    Expired,
    VersionCut,
}

impl PublicationReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Operator => "operator",
            Self::Expired => "expired",
            Self::VersionCut => "version_cut",
        }
    }
}

fn store_error(error: impl std::fmt::Display) -> PublicationRefusal {
    PublicationRefusal::Store(format!("{error:#}"))
}

fn is_hex(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Whether `value` is a lowercase SHA-256 digest.
pub fn is_publication_hash(value: &str) -> bool {
    is_hex(value, 64)
}

/// Whether `value` is a well-formed publication id: 32 lowercase hex digits.
pub fn is_publication_id(value: &str) -> bool {
    is_hex(value, 32)
}

// ── The record ──────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicationRoot {
    pub kind: String,
    pub manifest_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicationLimits {
    /// Largest response one read may carry.
    pub max_bytes_per_read: u64,
}

/// The publication record body: what the operator signs and the node
/// countersigns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicationRecord {
    pub schema: String,
    /// 32 lowercase hex digits.
    pub id: String,
    pub nonce: String,
    pub node_fingerprint: String,
    pub root: PublicationRoot,
    pub bundle_name: String,
    /// The bundle's own version, read from its signed manifest at freeze and
    /// never typed: publish refuses a record whose version is not the frozen
    /// manifest's.
    pub version: String,
    /// RFC 3339 UTC instant after which the node stops serving.
    pub not_after: String,
    pub created_at: String,
    pub limits: PublicationLimits,
    pub statement: String,
}

/// A bundle version: 1 to 64 printable ASCII bytes, no whitespace.
fn is_bundle_version(version: &str) -> bool {
    (1..=64).contains(&version.len()) && version.bytes().all(|b| b.is_ascii_graphic())
}

impl PublicationRecord {
    /// Build a record for the manifest at `manifest_hash`.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: &str,
        nonce: &str,
        node_fingerprint: &str,
        manifest_hash: &str,
        bundle_name: &str,
        version: &str,
        not_after: &str,
        created_at: &str,
        max_bytes_per_read: u64,
    ) -> Self {
        Self {
            schema: PUBLICATION_SCHEMA.to_owned(),
            id: id.to_owned(),
            nonce: nonce.to_owned(),
            node_fingerprint: node_fingerprint.to_owned(),
            root: PublicationRoot {
                kind: PUBLICATION_ROOT_BUNDLE_EXPORT.to_owned(),
                manifest_hash: manifest_hash.to_owned(),
            },
            bundle_name: bundle_name.to_owned(),
            version: version.to_owned(),
            not_after: not_after.to_owned(),
            created_at: created_at.to_owned(),
            limits: PublicationLimits { max_bytes_per_read },
            statement: PUBLICATION_STATEMENT.to_owned(),
        }
    }

    pub fn validate_shape(&self) -> Result<(), PublicationRefusal> {
        let malformed = |text: &str| Err(PublicationRefusal::Malformed(text.to_owned()));
        if self.schema != PUBLICATION_SCHEMA {
            return malformed("unknown schema");
        }
        if !is_publication_id(&self.id) {
            return malformed("id is not 32 lowercase hex digits");
        }
        if self.nonce.is_empty() || self.nonce.len() > 128 || self.nonce.contains('\0') {
            return malformed("nonce must be 1 to 128 bytes");
        }
        if !is_publication_hash(&self.node_fingerprint) {
            return malformed("node_fingerprint is not a fingerprint");
        }
        if self.root.kind != PUBLICATION_ROOT_BUNDLE_EXPORT
            || !is_publication_hash(&self.root.manifest_hash)
        {
            return malformed("root must be a bundle_export naming a manifest hash");
        }
        if self.bundle_name.is_empty()
            || self.bundle_name.len() > 128
            || self.bundle_name.contains(['\0', '/'])
        {
            return malformed("bundle_name must be 1 to 128 bytes without `/`");
        }
        if !is_bundle_version(&self.version) {
            return malformed("version must be 1 to 64 printable bytes");
        }
        if lillux::time::unix_secs_from_iso8601(&self.not_after).is_none()
            || lillux::time::unix_secs_from_iso8601(&self.created_at).is_none()
        {
            return malformed("not_after and created_at must be canonical UTC instants");
        }
        if !(MIN_PUBLICATION_READ_BYTES..=MAX_PUBLICATION_READ_BYTES)
            .contains(&self.limits.max_bytes_per_read)
        {
            return malformed("max_bytes_per_read is outside its bounds");
        }
        if self.statement != PUBLICATION_STATEMENT {
            return malformed("statement is not the publication statement");
        }
        Ok(())
    }

    fn canonical(&self) -> Result<String, PublicationRefusal> {
        let value = serde_json::to_value(self).map_err(store_error)?;
        lillux::canonical_json(&value)
            .map_err(|_| PublicationRefusal::Malformed("record is not canonicalizable".into()))
    }

    /// SHA-256 of the canonical record body, both signatures excluded. This is
    /// what a puller pins as `--expect`.
    pub fn body_hash(&self) -> Result<String, PublicationRefusal> {
        Ok(lillux::sha256_hex(self.canonical()?.as_bytes()))
    }

    /// The text the operator signs.
    pub fn operator_signing_text(&self) -> Result<String, PublicationRefusal> {
        Ok(format!("{OPERATOR_DOMAIN}\n{}", self.body_hash()?))
    }

    /// Sign with the operator's own key; the signature is base64.
    pub fn sign_operator(&self, key: &SigningKey) -> Result<String, PublicationRefusal> {
        let signature = key.sign(self.operator_signing_text()?.as_bytes());
        Ok(base64::engine::general_purpose::STANDARD.encode(signature.to_bytes()))
    }

    fn verify_operator(
        &self,
        signature: &str,
        key: &VerifyingKey,
    ) -> Result<(), PublicationRefusal> {
        if lillux::signature::verify_signature(&self.operator_signing_text()?, signature, key) {
            Ok(())
        } else {
            Err(PublicationRefusal::BadSignature)
        }
    }

    pub fn not_after_secs(&self) -> u64 {
        lillux::time::unix_secs_from_iso8601(&self.not_after).unwrap_or(0)
    }
}

/// Digest of an operator signature, as the countersignature binds it.
pub fn operator_signature_digest(signature: &str) -> String {
    lillux::sha256_hex(signature.as_bytes())
}

fn countersign_text(node_fingerprint: &str, body_hash: &str, operator_digest: &str) -> String {
    format!("{NODE_DOMAIN}\n{node_fingerprint}\n{body_hash}\n{operator_digest}")
}

/// The form a stranger is served: the body, its hash, the digest of the
/// operator signature the node verified, and the node countersignature.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServedPublication {
    pub record: PublicationRecord,
    pub record_hash: String,
    pub operator_signature_digest: String,
    pub node_signature: String,
}

impl ServedPublication {
    /// What a puller checks: the record's own node fingerprint is the pinned
    /// node key's, the hash is the body's, and the node countersignature
    /// verifies under the pinned node key.
    pub fn verify(&self, node_key: &VerifyingKey) -> Result<(), PublicationRefusal> {
        self.record.validate_shape()?;
        if self.record.node_fingerprint != lillux::signature::compute_fingerprint(node_key) {
            return Err(PublicationRefusal::BadSignature);
        }
        if self.record.body_hash()? != self.record_hash {
            return Err(PublicationRefusal::BadSignature);
        }
        let text = countersign_text(
            &self.record.node_fingerprint,
            &self.record_hash,
            &self.operator_signature_digest,
        );
        if lillux::signature::verify_signature(&text, &self.node_signature, node_key) {
            Ok(())
        } else {
            Err(PublicationRefusal::BadSignature)
        }
    }
}

/// A publication as the node stores it, inside a node-signed file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredPublication {
    pub schema_version: u32,
    /// Fingerprint of the operator whose signature the node verified. Never
    /// served.
    pub operator: String,
    /// Base64 operator signature. Never served.
    pub operator_signature: String,
    pub node_signature: String,
    pub received_at: String,
    pub record: PublicationRecord,
}

impl StoredPublication {
    pub fn served(&self) -> Result<ServedPublication, PublicationRefusal> {
        Ok(ServedPublication {
            record: self.record.clone(),
            record_hash: self.record.body_hash()?,
            operator_signature_digest: operator_signature_digest(&self.operator_signature),
            node_signature: self.node_signature.clone(),
        })
    }

    /// The read-time check: the recorded signer is still an active operator
    /// and its signature still verifies. A retired or revoked operator key
    /// stops the node serving until the operator signs again.
    pub fn check_operator(&self, trust: &TrustStore) -> Result<(), PublicationRefusal> {
        let record = trust
            .signer(&self.operator)
            .filter(|record| {
                record.role == SignerRole::Operator && record.status == SignerStatus::Active
            })
            .ok_or(PublicationRefusal::SignerNotOperator)?;
        let key = record.verifying_key().map_err(store_error)?;
        self.record
            .verify_operator(&self.operator_signature, &key)
            .map_err(|_| PublicationRefusal::SignerNotOperator)
    }

    fn verify_stored(&self, node_key: &VerifyingKey) -> Result<(), PublicationRefusal> {
        self.served()?.verify(node_key)
    }
}

/// The evidence that one publication was retired.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicationTombstone {
    pub schema_version: u32,
    pub id: String,
    pub retired_at: String,
    pub reason: PublicationReason,
}

// ── The frozen manifest and its closure ─────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestEntry {
    pub path: String,
    pub hash: String,
    pub size: u64,
    pub mode: u32,
}

/// The export manifest object: bundle-relative paths and content hashes,
/// with no host path and no live pointer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicationManifest {
    pub kind: String,
    pub schema: String,
    pub bundle_name: String,
    /// The version in the bundle's signed manifest.
    pub bundle_version: String,
    pub bundle_manifest_digest: String,
    pub publisher_fingerprint: String,
    pub file_count: u64,
    pub total_bytes: u64,
    /// Sorted by path.
    pub entries: Vec<ManifestEntry>,
}

/// The hashes a publication may serve: the manifest hash and every entry
/// hash, with each entry blob's size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Closure {
    manifest_hash: String,
    sizes: BTreeMap<String, u64>,
}

impl Closure {
    pub fn manifest_hash(&self) -> &str {
        &self.manifest_hash
    }

    pub fn contains(&self, hash: &str) -> bool {
        hash == self.manifest_hash || self.sizes.contains_key(hash)
    }

    pub fn is_manifest(&self, hash: &str) -> bool {
        hash == self.manifest_hash
    }

    /// The size of an entry blob; `None` for the manifest or an unknown hash.
    pub fn blob_size(&self, hash: &str) -> Option<u64> {
        self.sizes.get(hash).copied()
    }

    pub fn largest_blob(&self) -> u64 {
        self.sizes.values().copied().max().unwrap_or(0)
    }

    /// How many distinct hashes the closure holds, the manifest included.
    pub fn hash_count(&self) -> usize {
        self.sizes.len() + 1
    }
}

impl PublicationManifest {
    pub fn validate_shape(&self) -> Result<(), PublicationRefusal> {
        let malformed = |text: &str| Err(PublicationRefusal::Malformed(text.to_owned()));
        if self.kind != PUBLICATION_MANIFEST_KIND || self.schema != PUBLICATION_MANIFEST_SCHEMA {
            return malformed("manifest has an unknown kind or schema");
        }
        if !is_bundle_version(&self.bundle_version) {
            return malformed("bundle_version must be 1 to 64 printable bytes");
        }
        if self.entries.len() > MAX_PUBLICATION_MANIFEST_ENTRIES
            || self.file_count != self.entries.len() as u64
        {
            return malformed("manifest file_count does not match its entries");
        }
        let mut paths = BTreeSet::new();
        let mut total = 0_u64;
        for entry in &self.entries {
            if !is_publication_hash(&entry.hash)
                || entry.path.is_empty()
                || entry.path.starts_with('/')
                || entry
                    .path
                    .split('/')
                    .any(|part| part == ".." || part == ".")
            {
                return malformed("manifest has an invalid entry");
            }
            if !paths.insert(entry.path.as_str()) {
                return malformed("manifest names a path twice");
            }
            total = total.saturating_add(entry.size);
        }
        if total != self.total_bytes {
            return malformed("manifest total_bytes does not match its entries");
        }
        Ok(())
    }

    /// Verify `bytes` are the object `manifest_hash` names, parse them and
    /// derive the closure.
    pub fn from_object_bytes(
        manifest_hash: &str,
        bytes: &[u8],
    ) -> Result<(Self, Closure), PublicationRefusal> {
        if lillux::sha256_hex(bytes) != manifest_hash {
            return Err(PublicationRefusal::Malformed(
                "manifest bytes do not match their hash".into(),
            ));
        }
        let manifest: Self = serde_json::from_slice(bytes)
            .map_err(|e| PublicationRefusal::Malformed(format!("manifest: {e}")))?;
        manifest.validate_shape()?;
        let sizes = manifest
            .entries
            .iter()
            .map(|entry| (entry.hash.clone(), entry.size))
            .collect();
        Ok((
            manifest,
            Closure {
                manifest_hash: manifest_hash.to_owned(),
                sizes,
            },
        ))
    }
}

// ── The store ───────────────────────────────────────────────────────

/// `<state>/publications` for the node at `app_root`.
pub fn publications_dir(app_root: &Path) -> PathBuf {
    RuntimeRoot::new(app_root.to_path_buf())
        .state()
        .join(PUBLICATIONS_DIR)
}

/// Where the rolling served-bytes journal lives. It is outside the signed
/// store: it holds counters, not authority.
pub fn served_ledger_path(app_root: &Path) -> PathBuf {
    RuntimeRoot::new(app_root.to_path_buf())
        .state()
        .join("publication-served.ledger")
}

/// One publication's stored record and tombstone, each read by id.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PublicationState {
    pub stored: Option<StoredPublication>,
    pub tombstone: Option<PublicationTombstone>,
}

/// What a publication is now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublicationStatus<'a> {
    Live(&'a StoredPublication),
    Expired(&'a StoredPublication),
    Retired {
        retired_at: String,
        reason: PublicationReason,
    },
    Unknown,
}

impl PublicationState {
    /// A tombstone wins over a record, so a put-back record stays retired.
    pub fn status(&self, now: u64) -> PublicationStatus<'_> {
        if let Some(tombstone) = &self.tombstone {
            return PublicationStatus::Retired {
                retired_at: tombstone.retired_at.clone(),
                reason: tombstone.reason,
            };
        }
        match &self.stored {
            None => PublicationStatus::Unknown,
            Some(stored) if stored.record.not_after_secs() <= now => {
                PublicationStatus::Expired(stored)
            }
            Some(stored) => PublicationStatus::Live(stored),
        }
    }

    /// The live record, or the refusal that says why there is none.
    pub fn live(&self, now: u64) -> Result<&StoredPublication, PublicationRefusal> {
        match self.status(now) {
            PublicationStatus::Live(stored) => Ok(stored),
            PublicationStatus::Expired(_) => Err(PublicationRefusal::Expired),
            PublicationStatus::Retired { retired_at, reason } => {
                Err(PublicationRefusal::Retired { retired_at, reason })
            }
            PublicationStatus::Unknown => Err(PublicationRefusal::NotFound),
        }
    }
}

fn read_node_signed(
    path: &Path,
    node_key: &VerifyingKey,
) -> Result<Option<String>, PublicationRefusal> {
    let bytes = match std::fs::File::open(path) {
        Ok(file) => lillux::read_open_regular_file_bounded(file, MAX_TRUST_DOCUMENT_BYTES)
            .map_err(store_error)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(store_error(error)),
    };
    verify_node_signed_text(path, &bytes, node_key)
        .map(Some)
        .map_err(store_error)
}

fn read_stored(
    path: &Path,
    id: &str,
    node_key: &VerifyingKey,
) -> Result<Option<StoredPublication>, PublicationRefusal> {
    let Some(text) = read_node_signed(path, node_key)? else {
        return Ok(None);
    };
    let stored: StoredPublication = serde_json::from_str(&text).map_err(store_error)?;
    stored.record.validate_shape()?;
    if stored.schema_version != STORE_SCHEMA_VERSION || stored.record.id != id {
        return Err(PublicationRefusal::Store(format!(
            "{} names another publication or schema",
            path.display()
        )));
    }
    stored.verify_stored(node_key)?;
    Ok(Some(stored))
}

fn read_tombstone(
    path: &Path,
    id: &str,
    node_key: &VerifyingKey,
) -> Result<Option<PublicationTombstone>, PublicationRefusal> {
    let Some(text) = read_node_signed(path, node_key)? else {
        return Ok(None);
    };
    let tombstone: PublicationTombstone = serde_json::from_str(&text).map_err(store_error)?;
    if tombstone.schema_version != STORE_SCHEMA_VERSION || tombstone.id != id {
        return Err(PublicationRefusal::Store(format!(
            "{} names another publication or schema",
            path.display()
        )));
    }
    Ok(Some(tombstone))
}

/// Read one publication's record and tombstone by id. A document that is
/// unsigned, signed by another key, edited, misnamed or malformed is a refusal.
pub fn load_publication(
    app_root: &Path,
    node_key: &VerifyingKey,
    id: &str,
) -> Result<PublicationState, PublicationRefusal> {
    if !is_publication_id(id) {
        return Ok(PublicationState::default());
    }
    let root = publications_dir(app_root);
    Ok(PublicationState {
        stored: read_stored(&root.join(format!("{id}.json")), id, node_key)?,
        tombstone: read_tombstone(
            &root.join(RETIRED_DIR).join(format!("{id}.json")),
            id,
            node_key,
        )?,
    })
}

/// Every verified document in the store.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PublicationLedger {
    pub publications: Vec<StoredPublication>,
    pub tombstones: Vec<PublicationTombstone>,
}

impl PublicationLedger {
    pub fn load(app_root: &Path, node_key: &VerifyingKey) -> Result<Self, PublicationRefusal> {
        let root = publications_dir(app_root);
        let mut files: Vec<(PathBuf, bool)> = Vec::new();
        lillux::visit_regular_files_no_follow_bounded(
            &root,
            lillux::DirectoryTraversalBudget::new(MAX_TRUST_TRAVERSAL_ENTRIES, 1),
            |_, _| Ok(false),
            |relative, _file| {
                if files.len() >= MAX_TRUST_DOCUMENTS {
                    anyhow::bail!("the publication store exceeds {MAX_TRUST_DOCUMENTS} documents");
                }
                let retired = relative
                    .parent()
                    .and_then(Path::file_name)
                    .and_then(OsStr::to_str)
                    == Some(RETIRED_DIR);
                files.push((root.join(relative), retired));
                Ok(())
            },
        )
        .map_err(store_error)?;
        files.sort();
        let mut ledger = Self::default();
        for (path, retired) in files {
            let id = path
                .file_name()
                .and_then(OsStr::to_str)
                .and_then(|name| name.strip_suffix(".json"))
                .filter(|id| is_publication_id(id))
                .ok_or_else(|| {
                    PublicationRefusal::Store(format!(
                        "{} is not an `<id>.json` document",
                        path.display()
                    ))
                })?
                .to_owned();
            if retired {
                if let Some(tombstone) = read_tombstone(&path, &id, node_key)? {
                    ledger.tombstones.push(tombstone);
                }
            } else if let Some(stored) = read_stored(&path, &id, node_key)? {
                ledger.publications.push(stored);
            }
        }
        Ok(ledger)
    }

    /// Manifest hashes the content store must keep: those of publications that
    /// are neither retired nor past `not_after` at `now`. A manifest object
    /// reaches its entry blobs through its registered links, so these are the
    /// only roots a sweep needs. A retired or expired publication stops being
    /// a root, and its closure may be collected.
    pub fn pinned_manifest_hashes(&self, now: u64) -> Vec<String> {
        let mut hashes: Vec<String> = self
            .publications
            .iter()
            .filter(|stored| {
                matches!(
                    self.state_of(&stored.record.id).status(now),
                    PublicationStatus::Live(_)
                )
            })
            .map(|stored| stored.record.root.manifest_hash.clone())
            .collect();
        hashes.sort();
        hashes.dedup();
        hashes
    }

    pub fn state_of(&self, id: &str) -> PublicationState {
        PublicationState {
            stored: self
                .publications
                .iter()
                .find(|p| p.record.id == id)
                .cloned(),
            tombstone: self.tombstones.iter().find(|t| t.id == id).cloned(),
        }
    }
}

fn write_node_signed(
    directory: &lillux::PinnedDirectory,
    name: &str,
    body: &str,
    node_key: &SigningKey,
) -> Result<(), PublicationRefusal> {
    let signed = lillux::signature::sign_content(body, node_key, "#", None);
    let _lock = directory.lock_exclusive().map_err(store_error)?;
    directory.ensure_path_binding().map_err(store_error)?;
    let incumbent = directory
        .open_pinned_regular(OsStr::new(name), false)
        .map_err(store_error)?;
    directory
        .replace_pinned_bytes_if_matches_atomic(
            OsStr::new(name),
            incumbent.as_ref(),
            |_| Ok(()),
            signed.as_bytes(),
            0o600,
        )
        .map_err(store_error)
}

fn child_directory(
    app_root: &Path,
    child: Option<&str>,
) -> Result<lillux::PinnedDirectory, PublicationRefusal> {
    let root = lillux::PinnedDirectory::open_or_create(&publications_dir(app_root))
        .map_err(store_error)?;
    match child {
        None => Ok(root),
        Some(name) => root
            .open_or_create_child(OsStr::new(name), 0o700)
            .map_err(store_error),
    }
}

/// Check, countersign and store one publication the operator signed.
///
/// `operator` is the fingerprint of the key that signed: it must be an active
/// operator in `trust`, the signature must verify under that operator's pinned
/// key, the record must be for this node, `not_after` must be in the future
/// and within `max_lifetime_days` (itself capped by
/// [`PUBLICATION_HARD_MAX_LIFETIME_DAYS`]). The caller serializes writes and
/// has already checked that the manifest the record names is frozen.
#[allow(clippy::too_many_arguments)]
pub fn publish_record(
    app_root: &Path,
    node_key: &SigningKey,
    trust: &TrustStore,
    operator: &str,
    record: PublicationRecord,
    operator_signature: String,
    max_lifetime_days: u32,
    now: u64,
) -> Result<StoredPublication, PublicationRefusal> {
    record.validate_shape()?;
    let node_verifying = node_key.verifying_key();
    if record.node_fingerprint != lillux::signature::compute_fingerprint(&node_verifying) {
        return Err(PublicationRefusal::Malformed(
            "the record is for another node".into(),
        ));
    }
    let signer = trust
        .signer(operator)
        .filter(|signer| {
            signer.role == SignerRole::Operator && signer.status == SignerStatus::Active
        })
        .ok_or(PublicationRefusal::SignerNotOperator)?;
    let key = signer.verifying_key().map_err(store_error)?;
    record.verify_operator(&operator_signature, &key)?;

    let created = lillux::time::unix_secs_from_iso8601(&record.created_at).unwrap_or(0);
    if created.abs_diff(now) > MAX_PUBLICATION_CLOCK_SKEW_SECS {
        return Err(PublicationRefusal::Malformed(format!(
            "created_at is more than {MAX_PUBLICATION_CLOCK_SKEW_SECS} seconds from the node's \
             clock; sign again and check the signing machine's clock"
        )));
    }
    let not_after = record.not_after_secs();
    if not_after <= now {
        return Err(PublicationRefusal::Expired);
    }
    let max_days = max_lifetime_days.min(PUBLICATION_HARD_MAX_LIFETIME_DAYS);
    if not_after > now.saturating_add(u64::from(max_days) * 86_400) {
        return Err(PublicationRefusal::NotAfterTooFar { max_days });
    }

    let ledger = PublicationLedger::load(app_root, &node_verifying)?;
    let state = ledger.state_of(&record.id);
    if state.tombstone.is_some() {
        return Err(PublicationRefusal::AlreadyExists);
    }
    if let Some(existing) = state.stored {
        if existing.record == record && existing.operator_signature == operator_signature {
            return Ok(existing);
        }
        return Err(PublicationRefusal::AlreadyExists);
    }
    if ledger.publications.len() + ledger.tombstones.len() >= MAX_PUBLICATIONS {
        return Err(PublicationRefusal::Store(format!(
            "the store holds {MAX_PUBLICATIONS} publications; retired ones are kept, so none can \
             be added"
        )));
    }

    let body_hash = record.body_hash()?;
    let text = countersign_text(
        &record.node_fingerprint,
        &body_hash,
        &operator_signature_digest(&operator_signature),
    );
    let node_signature =
        base64::engine::general_purpose::STANDARD.encode(node_key.sign(text.as_bytes()).to_bytes());
    let stored = StoredPublication {
        schema_version: STORE_SCHEMA_VERSION,
        operator: operator.to_owned(),
        operator_signature,
        node_signature,
        received_at: lillux::time::iso8601_from_unix_secs(now),
        record,
    };
    let json = serde_json::to_string(&stored).map_err(store_error)?;
    let directory = child_directory(app_root, None)?;
    write_node_signed(
        &directory,
        &format!("{}.json", stored.record.id),
        &json,
        node_key,
    )?;
    directory.sync().map_err(store_error)?;
    Ok(stored)
}

/// Write the tombstone of a stored publication. Returns the tombstone and
/// whether this call wrote it: the first retirement is the one that counts.
pub fn retire_publication(
    app_root: &Path,
    node_key: &SigningKey,
    id: &str,
    reason: PublicationReason,
    now: u64,
) -> Result<(PublicationTombstone, bool), PublicationRefusal> {
    if !is_publication_id(id) {
        return Err(PublicationRefusal::Malformed(
            "id is not 32 lowercase hex digits".into(),
        ));
    }
    let state = load_publication(app_root, &node_key.verifying_key(), id)?;
    if let Some(existing) = state.tombstone {
        return Ok((existing, false));
    }
    if state.stored.is_none() {
        return Err(PublicationRefusal::NotFound);
    }
    let tombstone = PublicationTombstone {
        schema_version: STORE_SCHEMA_VERSION,
        id: id.to_owned(),
        retired_at: lillux::time::iso8601_from_unix_secs(now),
        reason,
    };
    let json = serde_json::to_string(&tombstone).map_err(store_error)?;
    let directory = child_directory(app_root, Some(RETIRED_DIR))?;
    write_node_signed(&directory, &format!("{id}.json"), &json, node_key)?;
    directory.sync().map_err(store_error)?;
    Ok((tombstone, true))
}

#[cfg(test)]
mod tests;
