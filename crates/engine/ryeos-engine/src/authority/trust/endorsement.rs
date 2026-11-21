//! Endorsements: operator-signed statements that make items the node's
//! authoring key signed trusted.
//!
//! The authoring key is pinned with no kinds, so nothing it signs is trusted
//! by its registry scope. An endorsement is a separate signed document that
//! leaves the item untouched and names what the operator approves:
//!
//! * a per-change endorsement names exact items: kind and content digest (the
//!   digest of the signed bytes after the signature line, which is what the
//!   engine recomputes when it verifies the item), so one changed byte
//!   returns the item to untrusted;
//! * a standing endorsement names kinds within one project, optionally until a
//!   time, and covers whatever the authoring key signs of those kinds there.
//!
//! The body is signed by the endorser's own key, client side. The node never
//! holds that key: it checks the signature against its signer registry (the
//! endorser must be a pinned operator), countersigns the document on receipt
//! and stores it. Revoking an endorsement writes a node-signed tombstone named
//! by the endorsement digest, which stays dead if the endorsement file is put
//! back.
//!
//! Trust only chooses between two classes: an endorsement can make an
//! authoring-key item trusted instead of untrusted and nothing else. It cannot
//! make an unpinned or revoked key trusted, lift a revocation, or widen what
//! trusted project items may do.
//!
//! Layout below `<state>/endorsements/`:
//!
//! ```text
//! <sha256 of project path>/<endorsement digest>.json   node-signed, one per endorsement
//! revoked/<endorsement digest>.json                    node-signed tombstone
//! ```

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use base64::Engine as _;
use lillux::crypto::{Signature, Signer as _, SigningKey, Verifier as _, VerifyingKey};
use serde::{Deserialize, Serialize};

use super::{
    MAX_TRUST_DOCUMENT_BYTES, MAX_TRUST_DOCUMENTS, MAX_TRUST_TRAVERSAL_ENTRIES, SignerRole,
    SignerStatus, TrustError, TrustStore, verify_node_signed_text,
};
use crate::roots::RuntimeRoot;

/// Schema tag of the signed endorsement body.
pub const ENDORSEMENT_SCHEMA: &str = "ryeos.endorsement.v1";
/// Domain separator of the signed bytes.
const SIGNING_DOMAIN: &str = "ryeos-endorsement-v1";
const STORE_SCHEMA_VERSION: u32 = 1;
/// Name of the directory below the state directory.
pub const ENDORSEMENTS_DIR: &str = "endorsements";
/// Directory of tombstones inside it.
pub const REVOKED_DIR: &str = "revoked";
/// Most items one per-change endorsement names.
pub const MAX_ENDORSED_ITEMS: usize = 256;
/// Most kinds one standing endorsement names.
pub const MAX_STANDING_KINDS: usize = 64;
/// Longest item ref, kind or project path an endorsement carries, in bytes.
pub const MAX_ENDORSEMENT_FIELD_BYTES: usize = 512;
/// Longest revocation reason, in bytes.
pub const MAX_REVOCATION_REASON_BYTES: usize = 1024;
/// How far an endorsement's own time may differ from the node's clock when the
/// node receives it, in seconds.
pub const MAX_ENDORSEMENT_CLOCK_SKEW_SECS: u64 = 120;

/// `<state>/endorsements` for the node at `app_root`.
pub fn endorsements_dir(app_root: &Path) -> PathBuf {
    RuntimeRoot::new(app_root.to_path_buf())
        .state()
        .join(ENDORSEMENTS_DIR)
}

/// Directory name of one project's endorsements.
pub fn project_directory_name(project: &str) -> String {
    lillux::sha256_hex(format!("ryeos/endorsement-project/v1:{project}").as_bytes())
}

/// Why an endorsement or a revocation was not accepted. Every refusal has a
/// stable code and names the cure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EndorsementRefusal {
    #[error("endorsement_malformed: {0}")]
    Malformed(String),
    #[error(
        "endorsement_bad_signature: the endorsement does not verify under the key {endorser} \
         names. Sign it again with the operator key"
    )]
    BadSignature { endorser: String },
    #[error(
        "endorsement_endorser_not_operator: {endorser} is not an active operator in this \
         node's signer registry. Only a pinned operator key may endorse; list signers with \
         `ryeos trust list`"
    )]
    EndorserNotOperator { endorser: String },
    #[error(
        "endorsement_delegation_unsupported: the endorsement names delegation {via}, which \
         this node does not honour. Sign the endorsement with an operator key directly"
    )]
    DelegationUnsupported { via: String },
    #[error(
        "endorsement_ledger_moved: the endorsement was signed against ledger head {signed} \
         but the node's ledger is at {current}. Another endorsement or revocation was \
         recorded first; sign again against the current head"
    )]
    LedgerMoved { signed: String, current: String },
    #[error(
        "endorsement_time_out_of_window: the endorsement says it was made at {endorsed_at}, \
         which is more than {MAX_ENDORSEMENT_CLOCK_SKEW_SECS} seconds from this node's clock \
         ({now}). Sign it again, and check the signing machine's clock"
    )]
    TimeOutOfWindow { endorsed_at: String, now: String },
    #[error(
        "endorsement_unknown: no endorsement {digest} is recorded on this node. List them \
         with `ryeos endorse list`"
    )]
    Unknown { digest: String },
    #[error("endorsement_store_unavailable: {0}")]
    Store(String),
}

impl EndorsementRefusal {
    /// Stable machine-readable code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Malformed(_) => "endorsement_malformed",
            Self::BadSignature { .. } => "endorsement_bad_signature",
            Self::EndorserNotOperator { .. } => "endorsement_endorser_not_operator",
            Self::DelegationUnsupported { .. } => "endorsement_delegation_unsupported",
            Self::LedgerMoved { .. } => "endorsement_ledger_moved",
            Self::TimeOutOfWindow { .. } => "endorsement_time_out_of_window",
            Self::Unknown { .. } => "endorsement_unknown",
            Self::Store(_) => "endorsement_store_unavailable",
        }
    }
}

/// One item a per-change endorsement approves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EndorsedItem {
    /// Canonical ref the approver reviewed. Recorded for the audit trail; the
    /// trust decision matches kind and digest.
    pub item_ref: String,
    pub kind: String,
    /// Digest of the signed bytes after the signature line (lowercase hex).
    pub digest: String,
}

/// What an endorsement covers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum EndorsementTarget {
    /// Exactly these items.
    Items { items: Vec<EndorsedItem> },
    /// Whatever the authoring key signs of these kinds in the project, until
    /// `not_after` when given.
    Standing {
        kinds: Vec<String>,
        not_after: Option<String>,
    },
}

/// The signed statement. Field order is the canonical order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EndorsementBody {
    pub schema: String,
    /// The project the endorsement is for: its absolute root path, as the
    /// registry names projects.
    pub project: String,
    /// `fp:<fingerprint>` of the key that signs the endorsement.
    pub endorser: String,
    /// Digest of the delegation the endorser signs under, when it signs for an
    /// operator rather than as one. The node does not honour a delegation yet,
    /// so a body that names one is refused.
    pub via: Option<String>,
    pub target: EndorsementTarget,
    /// When the endorser made the statement, RFC 3339 UTC. Inside the signed
    /// body: the time is part of what the signature vouches for.
    pub endorsed_at: String,
    /// Random 32 hex characters that make every endorsement distinct.
    pub nonce: String,
    /// The node's endorsement ledger head the endorser saw. Only one of two
    /// endorsements signed against the same head applies.
    pub ledger_head: String,
}

fn is_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn fingerprint_of(principal: &str) -> Option<&str> {
    principal.strip_prefix("fp:").filter(|fp| is_digest(fp))
}

fn bounded_text(label: &str, value: &str) -> Result<(), EndorsementRefusal> {
    if value.is_empty() || value.len() > MAX_ENDORSEMENT_FIELD_BYTES || value.contains('\0') {
        return Err(EndorsementRefusal::Malformed(format!(
            "{label} must be 1 to {MAX_ENDORSEMENT_FIELD_BYTES} bytes without NUL"
        )));
    }
    Ok(())
}

impl EndorsementBody {
    /// A per-change endorsement of exact items.
    pub fn for_items(
        project: &str,
        endorser: &str,
        items: Vec<EndorsedItem>,
        endorsed_at: &str,
        nonce: &str,
        ledger_head: &str,
    ) -> Self {
        Self::new(
            project,
            endorser,
            EndorsementTarget::Items { items },
            endorsed_at,
            nonce,
            ledger_head,
        )
    }

    /// A standing endorsement of kinds within a project.
    pub fn standing(
        project: &str,
        endorser: &str,
        kinds: Vec<String>,
        not_after: Option<String>,
        endorsed_at: &str,
        nonce: &str,
        ledger_head: &str,
    ) -> Self {
        Self::new(
            project,
            endorser,
            EndorsementTarget::Standing { kinds, not_after },
            endorsed_at,
            nonce,
            ledger_head,
        )
    }

    fn new(
        project: &str,
        endorser: &str,
        target: EndorsementTarget,
        endorsed_at: &str,
        nonce: &str,
        ledger_head: &str,
    ) -> Self {
        let target = match target {
            EndorsementTarget::Items { mut items } => {
                items.sort_by(|a, b| {
                    (&a.kind, &a.digest, &a.item_ref).cmp(&(&b.kind, &b.digest, &b.item_ref))
                });
                items.dedup();
                EndorsementTarget::Items { items }
            }
            EndorsementTarget::Standing {
                mut kinds,
                not_after,
            } => {
                kinds.sort();
                kinds.dedup();
                EndorsementTarget::Standing { kinds, not_after }
            }
        };
        Self {
            schema: ENDORSEMENT_SCHEMA.to_owned(),
            project: project.to_owned(),
            endorser: endorser.to_owned(),
            via: None,
            target,
            endorsed_at: endorsed_at.to_owned(),
            nonce: nonce.to_owned(),
            ledger_head: ledger_head.to_owned(),
        }
    }

    /// Structural validity: what every reader and writer of an endorsement
    /// requires, independent of any node state.
    pub fn validate_shape(&self) -> Result<(), EndorsementRefusal> {
        let malformed = |reason: String| Err(EndorsementRefusal::Malformed(reason));
        if self.schema != ENDORSEMENT_SCHEMA {
            return malformed(format!(
                "schema `{}` is not `{ENDORSEMENT_SCHEMA}`",
                self.schema
            ));
        }
        bounded_text("project", &self.project)?;
        if !Path::new(&self.project).is_absolute() {
            return malformed(format!(
                "project `{}` is not an absolute path",
                self.project
            ));
        }
        if fingerprint_of(&self.endorser).is_none() {
            return malformed("endorser is not a canonical `fp:` principal".to_owned());
        }
        if let Some(via) = &self.via
            && !is_digest(via)
        {
            return malformed("via is not a lowercase SHA-256 digest".to_owned());
        }
        let endorsed_at = lillux::time::unix_secs_from_iso8601(&self.endorsed_at);
        let Some(endorsed_at) = endorsed_at else {
            return malformed("endorsed_at is not a canonical UTC instant".to_owned());
        };
        if self.nonce.len() != 32 || !self.nonce.bytes().all(|b| b.is_ascii_hexdigit()) {
            return malformed("nonce is not 32 hex characters".to_owned());
        }
        if !is_digest(&self.ledger_head) {
            return malformed("ledger_head is not a lowercase SHA-256 digest".to_owned());
        }
        match &self.target {
            EndorsementTarget::Items { items } => {
                if items.is_empty() || items.len() > MAX_ENDORSED_ITEMS {
                    return malformed(format!(
                        "a per-change endorsement names 1 to {MAX_ENDORSED_ITEMS} items"
                    ));
                }
                for item in items {
                    bounded_text("item_ref", &item.item_ref)?;
                    bounded_text("kind", &item.kind)?;
                    if item.kind == super::ALL_KINDS {
                        return malformed("an item's kind cannot be `*`".to_owned());
                    }
                    if !is_digest(&item.digest) {
                        return malformed(format!(
                            "digest of `{}` is not a lowercase SHA-256 digest",
                            item.item_ref
                        ));
                    }
                }
                let mut seen = BTreeSet::new();
                for item in items {
                    if !seen.insert((&item.kind, &item.digest)) {
                        return malformed(format!(
                            "kind `{}` and digest {} are named twice",
                            item.kind, item.digest
                        ));
                    }
                }
            }
            EndorsementTarget::Standing { kinds, not_after } => {
                if kinds.is_empty() || kinds.len() > MAX_STANDING_KINDS {
                    return malformed(format!(
                        "a standing endorsement names 1 to {MAX_STANDING_KINDS} kinds"
                    ));
                }
                for kind in kinds {
                    bounded_text("kind", kind)?;
                    if kind == super::ALL_KINDS {
                        return malformed(
                            "a standing endorsement names kinds one by one, never `*`".to_owned(),
                        );
                    }
                }
                if kinds.windows(2).any(|pair| pair[0] >= pair[1]) {
                    return malformed("kinds are not sorted and unique".to_owned());
                }
                if let Some(not_after) = not_after {
                    match lillux::time::unix_secs_from_iso8601(not_after) {
                        Some(limit) if limit > endorsed_at => {}
                        Some(_) => {
                            return malformed("not_after is not after endorsed_at".to_owned());
                        }
                        None => {
                            return malformed(
                                "not_after is not a canonical UTC instant".to_owned(),
                            );
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// The endorser's fingerprint without the `fp:` prefix.
    pub fn endorser_fingerprint(&self) -> Result<&str, EndorsementRefusal> {
        fingerprint_of(&self.endorser).ok_or_else(|| {
            EndorsementRefusal::Malformed("endorser is not a canonical `fp:` principal".to_owned())
        })
    }

    fn signing_string(&self) -> Result<String, EndorsementRefusal> {
        let body = serde_json::to_string(self)
            .map_err(|error| EndorsementRefusal::Malformed(format!("serialize body: {error}")))?;
        Ok(format!("{SIGNING_DOMAIN}\n{body}"))
    }

    /// Content digest of the endorsement. It names the endorsement in the
    /// store, the revocation ledger and the node journal.
    pub fn digest(&self) -> Result<String, EndorsementRefusal> {
        Ok(lillux::sha256_hex(self.signing_string()?.as_bytes()))
    }

    /// The bytes the endorser signs: the digest as text.
    pub fn signing_bytes(&self) -> Result<Vec<u8>, EndorsementRefusal> {
        Ok(self.digest()?.into_bytes())
    }

    /// Whether the body covers one item's `(kind, digest)` at `now`.
    fn covers(&self, kind: &str, digest: &str, now: u64) -> bool {
        match &self.target {
            EndorsementTarget::Items { items } => items
                .iter()
                .any(|item| item.kind == kind && item.digest == digest),
            EndorsementTarget::Standing { kinds, not_after } => {
                kinds.iter().any(|candidate| candidate == kind)
                    && not_after
                        .as_deref()
                        .and_then(lillux::time::unix_secs_from_iso8601)
                        .is_none_or(|limit| now <= limit)
            }
        }
    }
}

/// An endorsement with the endorser's signature.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedEndorsement {
    pub body: EndorsementBody,
    /// Base64 Ed25519 signature over [`EndorsementBody::signing_bytes`].
    pub signature: String,
}

impl SignedEndorsement {
    /// Sign a body with the endorser's own key.
    pub fn sign(body: EndorsementBody, key: &SigningKey) -> Result<Self, EndorsementRefusal> {
        let signature = key.sign(&body.signing_bytes()?);
        Ok(Self::with_signature(
            body,
            base64::engine::general_purpose::STANDARD.encode(signature.to_bytes()),
        ))
    }

    /// Attach a signature computed elsewhere over the body's signing bytes.
    pub fn with_signature(body: EndorsementBody, signature: String) -> Self {
        Self { body, signature }
    }

    pub fn digest(&self) -> Result<String, EndorsementRefusal> {
        self.body.digest()
    }

    /// Verify the signature under `key`, which must be the key the body's
    /// endorser fingerprint names.
    pub fn verify(&self, key: &VerifyingKey) -> Result<(), EndorsementRefusal> {
        let bad = || EndorsementRefusal::BadSignature {
            endorser: self.body.endorser.clone(),
        };
        if lillux::signature::compute_fingerprint(key) != self.body.endorser_fingerprint()? {
            return Err(bad());
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&self.signature)
            .map_err(|_| bad())?;
        let signature = Signature::from_slice(&bytes).map_err(|_| bad())?;
        key.verify(&self.body.signing_bytes()?, &signature)
            .map_err(|_| bad())
    }
}

/// An endorsement as the node stores it: countersigned on receipt, with the
/// receipt time inside the node-signed body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredEndorsement {
    pub schema_version: u32,
    pub digest: String,
    pub received_at: String,
    pub endorsement: SignedEndorsement,
}

/// The evidence that one endorsement was withdrawn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EndorsementTombstone {
    pub schema_version: u32,
    /// Digest of the revoked endorsement.
    pub endorsement_digest: String,
    pub project: String,
    /// RFC 3339 UTC time of the revocation, inside the node-signed body.
    pub revoked_at: String,
    /// `fp:<fingerprint>` of the operator that revoked it.
    pub revoked_by: String,
    pub reason: String,
}

// ── The recorded ledger ─────────────────────────────────────────────

/// Every verified document in the store: endorsements and tombstones.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EndorsementLedger {
    pub endorsements: Vec<StoredEndorsement>,
    pub tombstones: Vec<EndorsementTombstone>,
}

fn node_fingerprint_of(node_key: &VerifyingKey) -> String {
    lillux::signature::compute_fingerprint(node_key)
}

impl EndorsementLedger {
    /// Read and verify every document under `<state>/endorsements`. A missing
    /// directory is an empty ledger. A document that is unsigned, signed by
    /// another key, edited, misnamed or malformed is a typed refusal: a
    /// ledger that cannot be trusted must not let an endorsement apply.
    pub fn load(app_root: &Path, node_key: &VerifyingKey) -> Result<Self, TrustError> {
        let root = endorsements_dir(app_root);
        let refusal = |path: &Path, reason: String| TrustError::EndorsementRecordInvalid {
            path: path.to_path_buf(),
            reason,
        };
        let mut files: Vec<(PathBuf, Vec<u8>)> = Vec::new();
        let mut total = 0_u64;
        lillux::visit_regular_files_no_follow_bounded(
            &root,
            lillux::DirectoryTraversalBudget::new(MAX_TRUST_TRAVERSAL_ENTRIES, 1),
            |_, _| Ok(false),
            |relative, file| {
                if files.len() >= MAX_TRUST_DOCUMENTS {
                    anyhow::bail!("the endorsement store exceeds {MAX_TRUST_DOCUMENTS} documents");
                }
                let bytes = lillux::read_open_regular_file_bounded(file, MAX_TRUST_DOCUMENT_BYTES)?;
                total = total.saturating_add(bytes.len() as u64);
                if total > super::MAX_TRUST_DIRECTORY_BYTES {
                    anyhow::bail!(
                        "the endorsement store exceeds {} aggregate bytes",
                        super::MAX_TRUST_DIRECTORY_BYTES
                    );
                }
                files.push((root.join(relative), bytes));
                Ok(())
            },
        )
        .map_err(|error| TrustError::Unreadable {
            path: root.clone(),
            reason: format!("{error:#}"),
        })?;
        files.sort_by(|a, b| a.0.cmp(&b.0));

        let mut ledger = Self::default();
        for (path, bytes) in files {
            let body = verify_node_signed_text(&path, &bytes, node_key)?;
            let name = path
                .file_name()
                .and_then(OsStr::to_str)
                .unwrap_or_default()
                .to_owned();
            let parent = path
                .parent()
                .and_then(Path::file_name)
                .and_then(OsStr::to_str)
                .unwrap_or_default()
                .to_owned();
            let digest = name.strip_suffix(".json").filter(|d| is_digest(d));
            let Some(digest) = digest else {
                return Err(refusal(
                    &path,
                    "is not a `<digest>.json` document".to_owned(),
                ));
            };
            if parent == REVOKED_DIR {
                let tombstone: EndorsementTombstone = serde_json::from_str(&body)
                    .map_err(|e| refusal(&path, format!("is not a tombstone: {e}")))?;
                if tombstone.schema_version != STORE_SCHEMA_VERSION
                    || tombstone.endorsement_digest != digest
                {
                    return Err(refusal(
                        &path,
                        "names a different endorsement digest or an unknown schema".to_owned(),
                    ));
                }
                ledger.tombstones.push(tombstone);
            } else {
                let stored: StoredEndorsement = serde_json::from_str(&body)
                    .map_err(|e| refusal(&path, format!("is not an endorsement: {e}")))?;
                let computed = stored
                    .endorsement
                    .digest()
                    .map_err(|e| refusal(&path, e.to_string()))?;
                if stored.schema_version != STORE_SCHEMA_VERSION
                    || stored.digest != digest
                    || computed != digest
                {
                    return Err(refusal(
                        &path,
                        "does not match the endorsement digest it is named by".to_owned(),
                    ));
                }
                if parent != project_directory_name(&stored.endorsement.body.project) {
                    return Err(refusal(
                        &path,
                        "is filed under another project's directory".to_owned(),
                    ));
                }
                stored
                    .endorsement
                    .body
                    .validate_shape()
                    .map_err(|e| refusal(&path, e.to_string()))?;
                ledger.endorsements.push(stored);
            }
        }
        ledger.endorsements.sort_by(|a, b| a.digest.cmp(&b.digest));
        ledger
            .tombstones
            .sort_by(|a, b| a.endorsement_digest.cmp(&b.endorsement_digest));
        Ok(ledger)
    }

    /// The ledger head: a digest over every recorded endorsement and
    /// tombstone, bound to the node. It moves with each acceptance and each
    /// revocation, which is what an endorsement signs against.
    pub fn head(&self, node_key: &VerifyingKey) -> String {
        let mut text = format!(
            "ryeos/endorsement-ledger/v1:{}\n",
            node_fingerprint_of(node_key)
        );
        for stored in &self.endorsements {
            text.push_str(&format!("e:{}\n", stored.digest));
        }
        for tombstone in &self.tombstones {
            text.push_str(&format!("t:{}\n", tombstone.endorsement_digest));
        }
        lillux::sha256_hex(text.as_bytes())
    }

    fn tombstone(&self, digest: &str) -> Option<&EndorsementTombstone> {
        self.tombstones
            .iter()
            .find(|tombstone| tombstone.endorsement_digest == digest)
    }

    /// Whether the endorsement is withdrawn.
    pub fn is_revoked(&self, digest: &str) -> bool {
        self.tombstone(digest).is_some()
    }
}

// ── The index the trust decision reads ──────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
struct IndexedEndorsement {
    digest: String,
    body: EndorsementBody,
    stored: StoredEndorsement,
}

/// The endorsements one launch carries to its runtime: the live endorsements
/// of the launch's project as the request froze them. The daemon attaches it to
/// the authoring-role signers of the launch's signer list, so every config
/// loader and the runtime judge exactly what the daemon judged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EndorsementSnapshot {
    /// The launch's logical project identity: its canonical root path. The
    /// runtime matches against this, never against its own checkout path.
    pub project: String,
    /// The stored endorsements, countersigned by the node, in digest order.
    pub endorsements: Vec<StoredEndorsement>,
}

/// The live endorsements the trust decision consults: verified, signed by a
/// registry operator that is not revoked, and not withdrawn.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EndorsementIndex {
    entries: Vec<IndexedEndorsement>,
}

impl EndorsementIndex {
    pub fn empty() -> Self {
        Self::default()
    }

    /// Build the index from a ledger against the registry in `trust`. An
    /// endorsement signed by a key that is not a pinned operator, or whose
    /// operator is revoked, or that names a delegation, is left out; one whose
    /// signature does not verify under the operator it names is a refusal.
    pub fn build(ledger: &EndorsementLedger, trust: &TrustStore) -> Result<Self, TrustError> {
        let mut entries = Vec::new();
        for stored in &ledger.endorsements {
            let body = &stored.endorsement.body;
            if ledger.is_revoked(&stored.digest) || body.via.is_some() {
                continue;
            }
            let Ok(endorser) = body.endorser_fingerprint() else {
                continue;
            };
            let Some(record) = trust.signer(endorser) else {
                continue;
            };
            if record.role != SignerRole::Operator || record.status == SignerStatus::Revoked {
                continue;
            }
            let key =
                record
                    .verifying_key()
                    .map_err(|reason| TrustError::EndorsementRecordInvalid {
                        path: PathBuf::from(format!("{}.json", stored.digest)),
                        reason,
                    })?;
            stored.endorsement.verify(&key).map_err(|error| {
                TrustError::EndorsementRecordInvalid {
                    path: PathBuf::from(format!("{}.json", stored.digest)),
                    reason: error.to_string(),
                }
            })?;
            entries.push(IndexedEndorsement {
                digest: stored.digest.clone(),
                body: body.clone(),
                stored: stored.clone(),
            });
        }
        entries.sort_by(|a, b| a.digest.cmp(&b.digest));
        Ok(Self { entries })
    }

    /// Whether a live endorsement covers an item of `kind` with content
    /// `digest` in `project` at `now` (unix seconds).
    pub fn covers(&self, project: &str, kind: &str, digest: &str, now: u64) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.body.project == project && entry.body.covers(kind, digest, now))
    }

    /// The live endorsements of `project`, for a launch's signer list. `None`
    /// when none apply.
    pub fn snapshot_for(&self, project: &str) -> Option<EndorsementSnapshot> {
        let endorsements: Vec<StoredEndorsement> = self
            .entries
            .iter()
            .filter(|entry| entry.body.project == project)
            .map(|entry| entry.stored.clone())
            .collect();
        if endorsements.is_empty() {
            return None;
        }
        Some(EndorsementSnapshot {
            project: project.to_owned(),
            endorsements,
        })
    }

    /// Rebuild the index a launch carries. The daemon already judged who may
    /// endorse and sealed the snapshot; this checks that each document is
    /// well formed, names its own digest and the snapshot's project, and
    /// carries a valid signature under a key in `key_of`.
    pub fn from_snapshot(
        snapshot: &EndorsementSnapshot,
        key_of: impl Fn(&str) -> Option<VerifyingKey>,
    ) -> Result<Self, String> {
        let mut entries = Vec::new();
        for stored in &snapshot.endorsements {
            let body = &stored.endorsement.body;
            body.validate_shape().map_err(|e| e.to_string())?;
            let digest = stored.endorsement.digest().map_err(|e| e.to_string())?;
            if digest != stored.digest || body.project != snapshot.project || body.via.is_some() {
                return Err(format!(
                    "endorsement {} does not belong in the snapshot of project {}",
                    stored.digest, snapshot.project
                ));
            }
            let endorser = body.endorser_fingerprint().map_err(|e| e.to_string())?;
            let key = key_of(endorser)
                .ok_or_else(|| format!("endorser {endorser} is not a signer of this launch"))?;
            stored.endorsement.verify(&key).map_err(|e| e.to_string())?;
            entries.push(IndexedEndorsement {
                digest,
                body: body.clone(),
                stored: stored.clone(),
            });
        }
        entries.sort_by(|a, b| a.digest.cmp(&b.digest));
        Ok(Self { entries })
    }

    /// Digests of the live endorsements, in order.
    pub fn digests(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|entry| entry.digest.as_str())
    }

    /// The endorsements in force for one project, in digest order.
    pub fn for_project<'a>(
        &'a self,
        project: &'a str,
    ) -> impl Iterator<Item = (&'a str, &'a EndorsementBody)> {
        self.entries
            .iter()
            .filter(move |entry| entry.body.project == project)
            .map(|entry| (entry.digest.as_str(), &entry.body))
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Identity of what the index trusts at `now`: the live endorsements that
    /// have not lapsed. It joins the trust store's fingerprint, so a request
    /// cache keyed on it retires when an endorsement is added, revoked or
    /// expires.
    pub fn identity(&self, now: u64) -> String {
        let mut text = String::from("ryeos/endorsement-index/v1\n");
        for entry in &self.entries {
            let lapsed = matches!(
                &entry.body.target,
                EndorsementTarget::Standing { not_after: Some(limit), .. }
                    if lillux::time::unix_secs_from_iso8601(limit).is_some_and(|limit| now > limit)
            );
            if !lapsed {
                text.push_str(&entry.digest);
                text.push('\n');
            }
        }
        lillux::sha256_hex(text.as_bytes())
    }
}

/// The trust store's handle on the endorsement index.
///
/// The store a daemon engine holds is live: the endorsement services replace
/// the index under it when one is recorded or revoked, so a running node
/// follows without a restart. Every request takes a [`snapshot`](Self::snapshot)
/// and sees one index for its whole life.
#[derive(Debug, Clone)]
pub(super) enum EndorsementView {
    Frozen(Arc<EndorsementIndex>),
    Live(Arc<RwLock<Arc<EndorsementIndex>>>),
}

impl Default for EndorsementView {
    fn default() -> Self {
        Self::Frozen(Arc::new(EndorsementIndex::empty()))
    }
}

impl EndorsementView {
    pub(super) fn live(index: EndorsementIndex) -> Self {
        Self::Live(Arc::new(RwLock::new(Arc::new(index))))
    }

    pub(super) fn frozen(index: EndorsementIndex) -> Self {
        Self::Frozen(Arc::new(index))
    }

    pub(super) fn current(&self) -> Arc<EndorsementIndex> {
        match self {
            Self::Frozen(index) => Arc::clone(index),
            Self::Live(slot) => match slot.read() {
                Ok(guard) => Arc::clone(&guard),
                Err(poisoned) => Arc::clone(&poisoned.into_inner()),
            },
        }
    }

    pub(super) fn snapshot(&self) -> Self {
        Self::Frozen(self.current())
    }

    /// Replace the index of a live view. A frozen view never changes.
    pub(super) fn replace(&self, index: EndorsementIndex) -> bool {
        match self {
            Self::Frozen(_) => false,
            Self::Live(slot) => {
                let mut guard = match slot.write() {
                    Ok(guard) => guard,
                    Err(poisoned) => poisoned.into_inner(),
                };
                *guard = Arc::new(index);
                true
            }
        }
    }
}

// ── Writing the store ───────────────────────────────────────────────

fn store_error(error: impl std::fmt::Display) -> EndorsementRefusal {
    EndorsementRefusal::Store(format!("{error:#}"))
}

fn write_node_signed(
    directory: &lillux::PinnedDirectory,
    name: &str,
    body: &str,
    node_key: &SigningKey,
) -> Result<(), EndorsementRefusal> {
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
    name: &str,
) -> Result<lillux::PinnedDirectory, EndorsementRefusal> {
    let root = lillux::PinnedDirectory::open_or_create(&endorsements_dir(app_root))
        .map_err(store_error)?;
    root.open_or_create_child(OsStr::new(name), 0o700)
        .map_err(store_error)
}

/// What recording an endorsement did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recorded {
    pub digest: String,
    pub stored: StoredEndorsement,
    /// False when the identical endorsement was already recorded.
    pub newly_recorded: bool,
    /// The ledger head after the write.
    pub ledger_head: String,
}

/// Check, countersign and store one endorsement the endorser signed.
///
/// The endorser must be an active operator in `trust`; the signature must
/// verify under that operator's pinned key; the body's time must be within the
/// clock skew of `now`; and the body must have been signed against the ledger
/// head as it stands. The caller serializes recording and revoking: this
/// function reads the ledger and writes without holding it.
pub fn record_endorsement(
    app_root: &Path,
    node_key: &SigningKey,
    trust: &TrustStore,
    signed: SignedEndorsement,
    now: u64,
) -> Result<Recorded, EndorsementRefusal> {
    let body = &signed.body;
    body.validate_shape()?;
    if let Some(via) = &body.via {
        return Err(EndorsementRefusal::DelegationUnsupported { via: via.clone() });
    }
    let endorser = body.endorser_fingerprint()?.to_owned();
    let not_operator = || EndorsementRefusal::EndorserNotOperator {
        endorser: body.endorser.clone(),
    };
    let record = trust.signer(&endorser).ok_or_else(not_operator)?;
    if record.role != SignerRole::Operator || record.status != SignerStatus::Active {
        return Err(not_operator());
    }
    let key = record.verifying_key().map_err(store_error)?;
    signed.verify(&key)?;
    let endorsed_at = lillux::time::unix_secs_from_iso8601(&body.endorsed_at)
        .ok_or_else(|| EndorsementRefusal::Malformed("endorsed_at is not a time".to_owned()))?;
    if endorsed_at.abs_diff(now) > MAX_ENDORSEMENT_CLOCK_SKEW_SECS {
        return Err(EndorsementRefusal::TimeOutOfWindow {
            endorsed_at: body.endorsed_at.clone(),
            now: lillux::time::iso8601_from_unix_secs(now),
        });
    }
    if let EndorsementTarget::Standing {
        not_after: Some(limit),
        ..
    } = &body.target
        && lillux::time::unix_secs_from_iso8601(limit).is_some_and(|limit| limit < now)
    {
        return Err(EndorsementRefusal::Malformed(
            "not_after has already passed".to_owned(),
        ));
    }

    let node_verifying = node_key.verifying_key();
    let ledger = EndorsementLedger::load(app_root, &node_verifying)
        .map_err(|error| EndorsementRefusal::Store(error.to_string()))?;
    let digest = signed.digest()?;
    if ledger.is_revoked(&digest) {
        return Err(EndorsementRefusal::Malformed(format!(
            "endorsement {digest} was revoked on this node; sign a new one"
        )));
    }
    if let Some(existing) = ledger.endorsements.iter().find(|s| s.digest == digest) {
        return Ok(Recorded {
            digest,
            stored: existing.clone(),
            newly_recorded: false,
            ledger_head: ledger.head(&node_verifying),
        });
    }
    let current = ledger.head(&node_verifying);
    if body.ledger_head != current {
        return Err(EndorsementRefusal::LedgerMoved {
            signed: body.ledger_head.clone(),
            current,
        });
    }
    if ledger.endorsements.len() + ledger.tombstones.len() >= MAX_TRUST_DOCUMENTS {
        return Err(EndorsementRefusal::Store(format!(
            "the endorsement store holds {MAX_TRUST_DOCUMENTS} documents; revoke ones that are no \
             longer needed"
        )));
    }
    let stored = StoredEndorsement {
        schema_version: STORE_SCHEMA_VERSION,
        digest: digest.clone(),
        received_at: lillux::time::iso8601_from_unix_secs(now),
        endorsement: signed,
    };
    let text = serde_json::to_string(&stored).map_err(store_error)?;
    let directory = child_directory(
        app_root,
        &project_directory_name(&stored.endorsement.body.project),
    )?;
    write_node_signed(&directory, &format!("{digest}.json"), &text, node_key)?;
    directory.sync().map_err(store_error)?;
    let mut ledger = ledger;
    ledger.endorsements.push(stored.clone());
    ledger.endorsements.sort_by(|a, b| a.digest.cmp(&b.digest));
    Ok(Recorded {
        digest,
        stored,
        newly_recorded: true,
        ledger_head: ledger.head(&node_verifying),
    })
}

/// What revoking an endorsement did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Revoked {
    pub tombstone: EndorsementTombstone,
    /// False when the endorsement was already revoked: the first revocation
    /// is the one that counts.
    pub newly_revoked: bool,
    pub ledger_head: String,
}

/// Write the tombstone of a recorded endorsement. `revoked_by` is the
/// fingerprint of the operator that asked; it must be a pinned operator that
/// is not revoked. The caller serializes recording and revoking.
pub fn revoke_endorsement(
    app_root: &Path,
    node_key: &SigningKey,
    trust: &TrustStore,
    endorsement_digest: &str,
    revoked_by: &str,
    reason: &str,
    now: u64,
) -> Result<Revoked, EndorsementRefusal> {
    if !is_digest(endorsement_digest) {
        return Err(EndorsementRefusal::Malformed(
            "the endorsement digest is not a lowercase SHA-256 digest".to_owned(),
        ));
    }
    if reason.len() > MAX_REVOCATION_REASON_BYTES || reason.contains('\0') {
        return Err(EndorsementRefusal::Malformed(format!(
            "a revocation reason is at most {MAX_REVOCATION_REASON_BYTES} bytes"
        )));
    }
    let revoker = trust
        .signer(revoked_by)
        .filter(|record| {
            record.role == SignerRole::Operator && record.status != SignerStatus::Revoked
        })
        .ok_or_else(|| EndorsementRefusal::EndorserNotOperator {
            endorser: format!("fp:{revoked_by}"),
        })?;
    let node_verifying = node_key.verifying_key();
    let ledger = EndorsementLedger::load(app_root, &node_verifying)
        .map_err(|error| EndorsementRefusal::Store(error.to_string()))?;
    if let Some(existing) = ledger.tombstone(endorsement_digest) {
        return Ok(Revoked {
            tombstone: existing.clone(),
            newly_revoked: false,
            ledger_head: ledger.head(&node_verifying),
        });
    }
    let stored = ledger
        .endorsements
        .iter()
        .find(|stored| stored.digest == endorsement_digest)
        .ok_or_else(|| EndorsementRefusal::Unknown {
            digest: endorsement_digest.to_owned(),
        })?;
    let tombstone = EndorsementTombstone {
        schema_version: STORE_SCHEMA_VERSION,
        endorsement_digest: endorsement_digest.to_owned(),
        project: stored.endorsement.body.project.clone(),
        revoked_at: lillux::time::iso8601_from_unix_secs(now),
        revoked_by: format!("fp:{}", revoker.fingerprint),
        reason: reason.to_owned(),
    };
    let text = serde_json::to_string(&tombstone).map_err(store_error)?;
    let directory = child_directory(app_root, REVOKED_DIR)?;
    write_node_signed(
        &directory,
        &format!("{endorsement_digest}.json"),
        &text,
        node_key,
    )?;
    directory.sync().map_err(store_error)?;
    let mut ledger = ledger;
    ledger.tombstones.push(tombstone.clone());
    ledger
        .tombstones
        .sort_by(|a, b| a.endorsement_digest.cmp(&b.endorsement_digest));
    Ok(Revoked {
        tombstone,
        newly_revoked: true,
        ledger_head: ledger.head(&node_verifying),
    })
}

#[cfg(test)]
mod tests;
