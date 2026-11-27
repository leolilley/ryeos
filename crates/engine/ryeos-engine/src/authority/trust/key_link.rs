//! Key links: a record that makes one more key speak for a principal.
//!
//! A principal's own active key signs "this key is me", the added key signs
//! "I am that principal", and the node pins the pair by its own signature. The
//! node's owner resolver then reads the link as one more source behind
//! `owner_for` and `speaks_for` ([`super::SuccessionGraph::with_links`]). A
//! link carries ownership only: the linked key still needs its own grant, and
//! the grant is what carries scope.
//!
//! Rules:
//! * one owner per key: a key with a succession lineage never takes a link,
//!   and a key with two live links to different principals resolves to
//!   nothing (`owner_link_conflict`);
//! * `not_after` is required and at most [`MAX_LINK_LIFETIME_SECS`] after
//!   `created_at`; an expired link resolves to nothing;
//! * revoking a link writes a node-signed tombstone named by the link id, in
//!   force even if the link file is put back;
//! * later revoking the key that signed a link does not revoke the link: the
//!   link stays until it is tombstoned or expires.
//!
//! Layout below `<app_root>/.ai/node/trust/links/`:
//!
//! ```text
//! <link id>.json            node-signed, one per link
//! revoked/<link id>.json    node-signed tombstone
//! ```

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use lillux::crypto::{Signature, Signer as _, SigningKey, Verifier as _, VerifyingKey};
use ryeos_state::principal_id::PrincipalId;
use serde::{Deserialize, Serialize};

use super::{
    MAX_TRUST_DOCUMENT_BYTES, MAX_TRUST_DOCUMENTS, MAX_TRUST_TRAVERSAL_ENTRIES, NODE_TRUST_DIR,
    TrustError, verify_node_signed_text,
};

/// Schema tag of the signed link body.
pub const KEY_LINK_SCHEMA: &str = "ryeos/key-link/v1";
/// Domain separator of the signed bytes.
const SIGNING_DOMAIN: &str = "ryeos-key-link-v1";
const STORE_SCHEMA_VERSION: u32 = 1;
/// Directory of links below `<app_root>/.ai/node/trust`.
pub const KEY_LINKS_DIR: &str = "links";
/// Directory of tombstones inside it.
pub const KEY_LINK_REVOKED_DIR: &str = "revoked";
/// Longest a link may last, counted from its creation.
pub const MAX_LINK_LIFETIME_SECS: u64 = 365 * 24 * 60 * 60;
/// The lifetime a link into the node operator's owner gets by default.
pub const DEFAULT_OPERATOR_LINK_LIFETIME_SECS: u64 = 90 * 24 * 60 * 60;
/// Doctor warns this long before a link expires.
pub const LINK_EXPIRY_WARNING_SECS: u64 = 30 * 24 * 60 * 60;
/// Longest revocation reason, in bytes.
pub const MAX_LINK_REVOCATION_REASON_BYTES: usize = 1024;

/// `<app_root>/.ai/node/trust/links`.
pub fn key_links_dir(app_root: &Path) -> PathBuf {
    app_root
        .join(crate::AI_DIR)
        .join(NODE_TRUST_DIR)
        .join(KEY_LINKS_DIR)
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

/// Why a link was not accepted. Every refusal has a stable code.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KeyLinkRefusal {
    #[error("key_link_malformed: {0}")]
    Malformed(String),
    #[error(
        "key_link_bad_signature: the {side} signature does not verify under the key the \
         link names"
    )]
    BadSignature { side: &'static str },
    #[error("key_link_store: {0}")]
    Store(String),
}

/// The signed body of one link.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyLinkBody {
    pub schema: String,
    /// 32 hex characters, so two links of the same pair differ.
    pub nonce: String,
    /// `pid:v1:<hex>` of the principal the key is joined to.
    pub principal: String,
    /// `fp:<hex>` of the key being added.
    pub key: String,
    /// Base64 Ed25519 public key of `key`.
    pub key_public: String,
    /// `fp:<hex>` of the principal's own key that vouches for the link.
    pub linked_by: String,
    /// Base64 Ed25519 public key of `linked_by`.
    pub linked_by_public: String,
    /// RFC 3339 UTC.
    pub created_at: String,
    /// RFC 3339 UTC; required.
    pub not_after: String,
    /// Digest of the request that carried the link, for the receipt.
    pub request_digest: String,
}

impl KeyLinkBody {
    fn signing_string(&self) -> Result<String, KeyLinkRefusal> {
        let body = serde_json::to_string(self)
            .map_err(|error| KeyLinkRefusal::Malformed(format!("serialize body: {error}")))?;
        Ok(format!("{SIGNING_DOMAIN}\n{body}"))
    }

    /// The link id: the digest of the body. It names the file and the tombstone.
    pub fn id(&self) -> Result<String, KeyLinkRefusal> {
        Ok(lillux::sha256_hex(self.signing_string()?.as_bytes()))
    }

    /// The bytes both keys sign: the id as text.
    pub fn signing_bytes(&self) -> Result<Vec<u8>, KeyLinkRefusal> {
        Ok(self.id()?.into_bytes())
    }

    /// The added key's fingerprint without the `fp:` prefix.
    pub fn key_fingerprint(&self) -> Result<&str, KeyLinkRefusal> {
        fingerprint_of(&self.key)
            .ok_or_else(|| KeyLinkRefusal::Malformed("key is not a canonical `fp:` key".into()))
    }

    /// The linking key's fingerprint without the `fp:` prefix.
    pub fn linked_by_fingerprint(&self) -> Result<&str, KeyLinkRefusal> {
        fingerprint_of(&self.linked_by).ok_or_else(|| {
            KeyLinkRefusal::Malformed("linked_by is not a canonical `fp:` key".into())
        })
    }

    /// The principal as a typed id.
    pub fn principal_id(&self) -> Result<PrincipalId, KeyLinkRefusal> {
        self.principal.parse().map_err(|error| {
            KeyLinkRefusal::Malformed(format!("principal is not a `pid:v1:` id: {error:#}"))
        })
    }

    /// The instants as seconds since the Unix epoch.
    pub fn times(&self) -> Result<(u64, u64), KeyLinkRefusal> {
        let created = lillux::time::unix_secs_from_iso8601(&self.created_at).ok_or_else(|| {
            KeyLinkRefusal::Malformed("created_at is not a canonical UTC instant".into())
        })?;
        let not_after = lillux::time::unix_secs_from_iso8601(&self.not_after).ok_or_else(|| {
            KeyLinkRefusal::Malformed("not_after is not a canonical UTC instant".into())
        })?;
        Ok((created, not_after))
    }

    /// Shape checks that need no key: schema, canonical fields, lifetime.
    pub fn validate_shape(&self) -> Result<(), KeyLinkRefusal> {
        let malformed = |reason: &str| Err(KeyLinkRefusal::Malformed(reason.to_owned()));
        if self.schema != KEY_LINK_SCHEMA {
            return malformed("unknown schema");
        }
        if self.nonce.len() != 32 || !is_digest(&format!("{}{}", self.nonce, self.nonce)) {
            return malformed("nonce is not 32 lowercase hex characters");
        }
        if !is_digest(&self.request_digest) {
            return malformed("request_digest is not a lowercase SHA-256 digest");
        }
        self.principal_id()?;
        let key = self.key_fingerprint()?;
        let by = self.linked_by_fingerprint()?;
        if key == by {
            return malformed("a key cannot be linked by itself");
        }
        let (created, not_after) = self.times()?;
        if not_after <= created {
            return malformed("not_after is not after created_at");
        }
        if not_after - created > MAX_LINK_LIFETIME_SECS {
            return malformed("a link lasts at most 365 days");
        }
        Ok(())
    }
}

/// A link with both signatures.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedKeyLink {
    pub body: KeyLinkBody,
    /// Base64 signature by `linked_by`: "this key is me".
    pub linked_by_signature: String,
    /// Base64 signature by `key`: "I am that principal".
    pub key_signature: String,
}

fn verifying_key(encoded: &str, expected_fp: &str) -> Option<VerifyingKey> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .ok()?;
    let array: [u8; 32] = bytes.try_into().ok()?;
    let key = VerifyingKey::from_bytes(&array).ok()?;
    (lillux::signature::compute_fingerprint(&key) == expected_fp).then_some(key)
}

fn check_signature(
    key: &VerifyingKey,
    signature: &str,
    bytes: &[u8],
    side: &'static str,
) -> Result<(), KeyLinkRefusal> {
    let bad = || KeyLinkRefusal::BadSignature { side };
    let raw = base64::engine::general_purpose::STANDARD
        .decode(signature)
        .map_err(|_| bad())?;
    let signature = Signature::from_slice(&raw).map_err(|_| bad())?;
    key.verify(bytes, &signature).map_err(|_| bad())
}

/// One side's signature over a body, base64: what `ryeos key link-sign`
/// prints. The linking key and the added key each sign the same body id.
pub fn sign_link_side(body: &KeyLinkBody, key: &SigningKey) -> Result<String, KeyLinkRefusal> {
    let signature = key.sign(&body.signing_bytes()?);
    Ok(base64::engine::general_purpose::STANDARD.encode(signature.to_bytes()))
}

impl SignedKeyLink {
    /// Sign `body` with the linking key (`linked_by`) and the added key.
    pub fn sign(
        body: KeyLinkBody,
        linked_by: &SigningKey,
        key: &SigningKey,
    ) -> Result<Self, KeyLinkRefusal> {
        let bytes = body.signing_bytes()?;
        let encode = |signature: Signature| {
            base64::engine::general_purpose::STANDARD.encode(signature.to_bytes())
        };
        Ok(Self {
            linked_by_signature: encode(linked_by.sign(&bytes)),
            key_signature: encode(key.sign(&bytes)),
            body,
        })
    }

    /// The link id.
    pub fn id(&self) -> Result<String, KeyLinkRefusal> {
        self.body.id()
    }

    /// Verify both signatures under the public keys the body carries, which
    /// must hash to the fingerprints it names.
    pub fn verify(&self) -> Result<(), KeyLinkRefusal> {
        self.body.validate_shape()?;
        let bytes = self.body.signing_bytes()?;
        let key = verifying_key(&self.body.key_public, self.body.key_fingerprint()?)
            .ok_or(KeyLinkRefusal::BadSignature { side: "key" })?;
        let by = verifying_key(
            &self.body.linked_by_public,
            self.body.linked_by_fingerprint()?,
        )
        .ok_or(KeyLinkRefusal::BadSignature { side: "linked_by" })?;
        check_signature(&by, &self.linked_by_signature, &bytes, "linked_by")?;
        check_signature(&key, &self.key_signature, &bytes, "key")
    }
}

/// A link as the node stores it, with the time the node pinned it inside the
/// node-signed body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredKeyLink {
    pub schema_version: u32,
    pub id: String,
    pub received_at: String,
    pub link: SignedKeyLink,
}

impl StoredKeyLink {
    pub fn new(link: SignedKeyLink, received_at: String) -> Result<Self, KeyLinkRefusal> {
        Ok(Self {
            schema_version: STORE_SCHEMA_VERSION,
            id: link.id()?,
            received_at,
            link,
        })
    }
}

/// The evidence that one link was withdrawn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyLinkTombstone {
    pub schema_version: u32,
    pub link_id: String,
    pub revoked_at: String,
    /// `fp:<fingerprint>` of the key that asked.
    pub revoked_by: String,
    pub reason: String,
}

/// Every verified document in the links directory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeyLinkLedger {
    pub links: Vec<StoredKeyLink>,
    pub tombstones: Vec<KeyLinkTombstone>,
}

/// One link that is in force (not tombstoned), as the resolver reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyLinkEntry {
    pub id: String,
    pub principal: PrincipalId,
    /// The added key, bare hex.
    pub key: String,
    /// The linking key, bare hex.
    pub linked_by: String,
    pub created_at_secs: u64,
    pub not_after_secs: u64,
}

impl KeyLinkLedger {
    /// Read and verify every document under the links directory. A missing
    /// directory is an empty ledger. A document that is unsigned, signed by
    /// another key, edited, misnamed or malformed is a typed refusal: a
    /// ledger that cannot be trusted must not let a link apply.
    pub fn load(app_root: &Path, node_key: &VerifyingKey) -> Result<Self, TrustError> {
        let root = key_links_dir(app_root);
        let refusal = |path: &Path, reason: String| TrustError::KeyLinkRecordInvalid {
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
                    anyhow::bail!("the key-link store exceeds {MAX_TRUST_DOCUMENTS} documents");
                }
                let bytes = lillux::read_open_regular_file_bounded(file, MAX_TRUST_DOCUMENT_BYTES)?;
                total = total.saturating_add(bytes.len() as u64);
                if total > super::MAX_TRUST_DIRECTORY_BYTES {
                    anyhow::bail!(
                        "the key-link store exceeds {} aggregate bytes",
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
            let Some(id) = name.strip_suffix(".json").filter(|id| is_digest(id)) else {
                return Err(refusal(&path, "is not a `<id>.json` document".to_owned()));
            };
            if parent == KEY_LINK_REVOKED_DIR {
                let tombstone: KeyLinkTombstone = serde_json::from_str(&body)
                    .map_err(|e| refusal(&path, format!("is not a tombstone: {e}")))?;
                if tombstone.schema_version != STORE_SCHEMA_VERSION || tombstone.link_id != id {
                    return Err(refusal(
                        &path,
                        "names a different link id or an unknown schema".to_owned(),
                    ));
                }
                ledger.tombstones.push(tombstone);
            } else {
                if parent != KEY_LINKS_DIR {
                    return Err(refusal(
                        &path,
                        "is filed in an unknown directory".to_owned(),
                    ));
                }
                let stored: StoredKeyLink = serde_json::from_str(&body)
                    .map_err(|e| refusal(&path, format!("is not a key link: {e}")))?;
                let computed = stored
                    .link
                    .id()
                    .map_err(|e| refusal(&path, e.to_string()))?;
                if stored.schema_version != STORE_SCHEMA_VERSION
                    || stored.id != id
                    || computed != id
                {
                    return Err(refusal(
                        &path,
                        "does not match the link id it is named by".to_owned(),
                    ));
                }
                stored
                    .link
                    .verify()
                    .map_err(|e| refusal(&path, e.to_string()))?;
                ledger.links.push(stored);
            }
        }
        ledger.links.sort_by(|a, b| a.id.cmp(&b.id));
        ledger.tombstones.sort_by(|a, b| a.link_id.cmp(&b.link_id));
        Ok(ledger)
    }

    /// Whether the link `id` has a tombstone.
    pub fn is_revoked(&self, id: &str) -> bool {
        self.tombstones
            .iter()
            .any(|tombstone| tombstone.link_id == id)
    }

    /// The links in force: recorded and not tombstoned. Expiry is judged when
    /// the owner is resolved, so the entry carries its end.
    pub fn entries(&self) -> Result<Vec<KeyLinkEntry>, TrustError> {
        let mut entries = Vec::new();
        for stored in &self.links {
            if self.is_revoked(&stored.id) {
                continue;
            }
            let body = &stored.link.body;
            let invalid = |reason: String| TrustError::KeyLinkRecordInvalid {
                path: PathBuf::from(format!("links/{}.json", stored.id)),
                reason,
            };
            let (created_at_secs, not_after_secs) =
                body.times().map_err(|e| invalid(e.to_string()))?;
            entries.push(KeyLinkEntry {
                id: stored.id.clone(),
                principal: body.principal_id().map_err(|e| invalid(e.to_string()))?,
                key: body
                    .key_fingerprint()
                    .map_err(|e| invalid(e.to_string()))?
                    .to_owned(),
                linked_by: body
                    .linked_by_fingerprint()
                    .map_err(|e| invalid(e.to_string()))?
                    .to_owned(),
                created_at_secs,
                not_after_secs,
            });
        }
        Ok(entries)
    }
}

// ── Writing the store ───────────────────────────────────────────────

fn store_error(error: impl std::fmt::Display) -> KeyLinkRefusal {
    KeyLinkRefusal::Store(format!("{error:#}"))
}

fn write_node_signed(
    directory: &lillux::PinnedDirectory,
    name: &str,
    body: &str,
    node_key: &SigningKey,
) -> Result<(), KeyLinkRefusal> {
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

/// Pin a verified link: write it node-signed under its id. Idempotent for the
/// same link. The caller has checked who may link what; this writes.
pub fn write_key_link(
    app_root: &Path,
    node_key: &SigningKey,
    stored: &StoredKeyLink,
) -> Result<(), KeyLinkRefusal> {
    stored.link.verify()?;
    let id = stored.link.id()?;
    if stored.id != id {
        return Err(KeyLinkRefusal::Malformed(
            "the stored id is not the link's id".into(),
        ));
    }
    let text = serde_json::to_string(stored).map_err(store_error)?;
    let directory =
        lillux::PinnedDirectory::open_or_create(&key_links_dir(app_root)).map_err(store_error)?;
    write_node_signed(&directory, &format!("{id}.json"), &text, node_key)
}

/// Write the tombstone of a link. Idempotent: the first revocation counts.
pub fn revoke_key_link(
    app_root: &Path,
    node_key: &SigningKey,
    link_id: &str,
    revoked_by: &str,
    reason: &str,
    now_iso: &str,
) -> Result<(), KeyLinkRefusal> {
    if !is_digest(link_id) {
        return Err(KeyLinkRefusal::Malformed(
            "the link id is not a lowercase SHA-256 digest".into(),
        ));
    }
    if fingerprint_of(revoked_by).is_none() {
        return Err(KeyLinkRefusal::Malformed(
            "revoked_by is not a canonical `fp:` key".into(),
        ));
    }
    if reason.len() > MAX_LINK_REVOCATION_REASON_BYTES || reason.contains('\0') {
        return Err(KeyLinkRefusal::Malformed(format!(
            "a revocation reason is at most {MAX_LINK_REVOCATION_REASON_BYTES} bytes"
        )));
    }
    let tombstone = KeyLinkTombstone {
        schema_version: STORE_SCHEMA_VERSION,
        link_id: link_id.to_owned(),
        revoked_at: now_iso.to_owned(),
        revoked_by: revoked_by.to_owned(),
        reason: reason.to_owned(),
    };
    let text = serde_json::to_string(&tombstone).map_err(store_error)?;
    let links =
        lillux::PinnedDirectory::open_or_create(&key_links_dir(app_root)).map_err(store_error)?;
    let directory = links
        .open_or_create_child(OsStr::new(KEY_LINK_REVOKED_DIR), 0o700)
        .map_err(store_error)?;
    write_node_signed(&directory, &format!("{link_id}.json"), &text, node_key)
}

/// For test fixtures: pin a link that joins `added` to `principal`, vouched
/// for by `linked_by`, lasting `lifetime_secs` from now, and return its
/// entry for [`super::TrustStore::with_key_links`]. The link is written as
/// the node would (node-signed under the links directory), so a store loaded
/// from `app_root` after this call reads it. `principal` is the `pid:v1:` text
/// of the owner `linked_by` acts for; take it from `owner_for`, never from a
/// key.
#[cfg(any(test, feature = "test-support"))]
pub fn pin_link_for_tests(
    app_root: &Path,
    node_key: &SigningKey,
    linked_by: &SigningKey,
    added: &SigningKey,
    principal: &str,
    lifetime_secs: u64,
) -> Result<KeyLinkEntry, KeyLinkRefusal> {
    let now = lillux::time::unix_secs_now();
    let encode = |key: &SigningKey| {
        base64::engine::general_purpose::STANDARD.encode(key.verifying_key().to_bytes())
    };
    let fingerprint = |key: &SigningKey| {
        format!(
            "fp:{}",
            lillux::signature::compute_fingerprint(&key.verifying_key())
        )
    };
    let nonce: [u8; 16] = std::array::from_fn(|index| (now as u8).wrapping_add(index as u8));
    let body = KeyLinkBody {
        schema: KEY_LINK_SCHEMA.to_owned(),
        nonce: lillux::sha256_hex(&nonce)[..32].to_owned(),
        principal: principal.to_owned(),
        key: fingerprint(added),
        key_public: encode(added),
        linked_by: fingerprint(linked_by),
        linked_by_public: encode(linked_by),
        created_at: lillux::time::iso8601_from_unix_secs(now),
        not_after: lillux::time::iso8601_from_unix_secs(now + lifetime_secs),
        request_digest: lillux::sha256_hex(b"ryeos/key-link-test/v1"),
    };
    let link = SignedKeyLink::sign(body, linked_by, added)?;
    let stored = StoredKeyLink::new(link, lillux::time::iso8601_from_unix_secs(now))?;
    write_key_link(app_root, node_key, &stored)?;
    let ledger = KeyLinkLedger {
        links: vec![stored],
        tombstones: Vec::new(),
    };
    let mut entries = ledger
        .entries()
        .map_err(|error| KeyLinkRefusal::Store(error.to_string()))?;
    entries
        .pop()
        .ok_or_else(|| KeyLinkRefusal::Store("the pinned link produced no entry".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(byte: u8) -> SigningKey {
        SigningKey::from_bytes(&[byte; 32])
    }

    fn fp(key: &SigningKey) -> String {
        format!(
            "fp:{}",
            lillux::signature::compute_fingerprint(&key.verifying_key())
        )
    }

    fn public(key: &SigningKey) -> String {
        base64::engine::general_purpose::STANDARD.encode(key.verifying_key().to_bytes())
    }

    fn body(by: &SigningKey, added: &SigningKey, created: u64, lifetime: u64) -> KeyLinkBody {
        KeyLinkBody {
            schema: KEY_LINK_SCHEMA.to_owned(),
            nonce: "ab".repeat(16),
            principal: crate::trust::SuccessionGraph::default()
                .ownership_of(fp(by).trim_start_matches("fp:"))
                .unwrap()
                .principal
                .to_string(),
            key: fp(added),
            key_public: public(added),
            linked_by: fp(by),
            linked_by_public: public(by),
            created_at: lillux::time::iso8601_from_unix_secs(created),
            not_after: lillux::time::iso8601_from_unix_secs(created + lifetime),
            request_digest: "cd".repeat(32),
        }
    }

    #[test]
    fn a_link_signed_by_both_keys_verifies_and_one_signed_by_one_does_not() {
        let (operator, laptop, stranger) = (key(1), key(2), key(3));
        let link = SignedKeyLink::sign(
            body(&operator, &laptop, 1_800_000_000, 86_400),
            &operator,
            &laptop,
        )
        .unwrap();
        link.verify().unwrap();

        // The added key did not sign: someone else's signature in its place.
        let forged = SignedKeyLink::sign(
            body(&operator, &laptop, 1_800_000_000, 86_400),
            &operator,
            &stranger,
        )
        .unwrap();
        assert_eq!(
            forged.verify().unwrap_err(),
            KeyLinkRefusal::BadSignature { side: "key" }
        );
        // The linking key did not sign.
        let forged = SignedKeyLink::sign(
            body(&operator, &laptop, 1_800_000_000, 86_400),
            &stranger,
            &laptop,
        )
        .unwrap();
        assert_eq!(
            forged.verify().unwrap_err(),
            KeyLinkRefusal::BadSignature { side: "linked_by" }
        );
    }

    #[test]
    fn an_edited_body_no_longer_verifies() {
        let (operator, laptop) = (key(1), key(2));
        let mut link = SignedKeyLink::sign(
            body(&operator, &laptop, 1_800_000_000, 86_400),
            &operator,
            &laptop,
        )
        .unwrap();
        link.body.not_after = lillux::time::iso8601_from_unix_secs(1_800_000_000 + 86_400 * 300);
        assert!(link.verify().is_err());
    }

    #[test]
    fn the_shape_refuses_a_missing_or_overlong_lifetime_and_a_self_link() {
        let (operator, laptop) = (key(1), key(2));
        let good = body(&operator, &laptop, 1_800_000_000, 86_400);
        good.validate_shape().unwrap();

        let mut zero = good.clone();
        zero.not_after = zero.created_at.clone();
        assert!(zero.validate_shape().is_err());

        let long = body(
            &operator,
            &laptop,
            1_800_000_000,
            MAX_LINK_LIFETIME_SECS + 1,
        );
        assert!(long.validate_shape().is_err());
        body(&operator, &laptop, 1_800_000_000, MAX_LINK_LIFETIME_SECS)
            .validate_shape()
            .unwrap();

        let mut itself = good;
        itself.key = itself.linked_by.clone();
        itself.key_public = itself.linked_by_public.clone();
        assert!(itself.validate_shape().is_err());
    }

    #[test]
    fn a_pinned_link_loads_and_a_tombstone_removes_it_even_if_the_file_is_put_back() {
        let (operator, laptop) = (key(1), key(2));
        let node = key(9);
        let app_root = tempfile::TempDir::new().unwrap();
        let link = SignedKeyLink::sign(
            body(&operator, &laptop, 1_800_000_000, 86_400),
            &operator,
            &laptop,
        )
        .unwrap();
        let stored = StoredKeyLink::new(link, "2026-11-26T00:00:00Z".to_owned()).unwrap();
        write_key_link(app_root.path(), &node, &stored).unwrap();

        let ledger = KeyLinkLedger::load(app_root.path(), &node.verifying_key()).unwrap();
        let entries = ledger.entries().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, stored.id);
        assert_eq!(entries[0].key, fp(&laptop).trim_start_matches("fp:"));

        revoke_key_link(
            app_root.path(),
            &node,
            &stored.id,
            &fp(&operator),
            "lost laptop",
            "2026-11-26T01:00:00Z",
        )
        .unwrap();
        // Put the link file back: the tombstone still wins.
        write_key_link(app_root.path(), &node, &stored).unwrap();
        let ledger = KeyLinkLedger::load(app_root.path(), &node.verifying_key()).unwrap();
        assert!(ledger.is_revoked(&stored.id));
        assert!(ledger.entries().unwrap().is_empty());
    }

    #[test]
    fn a_link_file_signed_by_another_node_key_is_refused_at_load() {
        let (operator, laptop) = (key(1), key(2));
        let app_root = tempfile::TempDir::new().unwrap();
        let link = SignedKeyLink::sign(
            body(&operator, &laptop, 1_800_000_000, 86_400),
            &operator,
            &laptop,
        )
        .unwrap();
        let stored = StoredKeyLink::new(link, "2026-11-26T00:00:00Z".to_owned()).unwrap();
        write_key_link(app_root.path(), &key(8), &stored).unwrap();
        assert!(KeyLinkLedger::load(app_root.path(), &key(9).verifying_key()).is_err());
    }
}
