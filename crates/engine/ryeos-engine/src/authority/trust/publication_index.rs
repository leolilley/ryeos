//! The publication index: a node-signed, short-lived list of what a node's
//! live publications are, by bundle name and version, so a stranger can pull
//! by `name@version` instead of by a printed link.
//!
//! The index is derived, never authored: the node builds it from its live,
//! unretired, unexpired publications, and every field of an entry except the
//! lane and the index's own framing is copied from the record or the frozen
//! manifest the record binds. It cannot name anything the node does not serve.
//! Listing a publication here grants nothing: the puller still pins the node
//! key, checks the publisher against its own trust store, and runs the whole
//! existing pull on the record the entry names.
//!
//! The index carries a monotonic `sequence`, a `generated_at` and a short
//! `valid_until`, so an old index replayed by a network attacker is refused
//! even on first contact. The puller keeps a high-water per registry and
//! refuses a lower sequence, or the same sequence with a different digest.
//! Retiring a publication is judged by the record route's tombstone, not by
//! the index, which may lag a retire by at most its validity.

use base64::Engine as _;
use lillux::crypto::{Signer as _, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};

use super::publication::{is_publication_hash, is_publication_id};

/// Schema tag of the index body.
pub const PUBLICATION_INDEX_SCHEMA: &str = "ryeos.publication-index/v1";
const INDEX_DOMAIN: &str = "ryeos-publication-index-v1";
/// Most entries one index carries.
pub const MAX_INDEX_ENTRIES: usize = 256;
/// Most requirements one entry carries.
pub const MAX_INDEX_REQUIRES: usize = 32;
/// Largest index a puller reads, in bytes.
pub const MAX_INDEX_BYTES: u64 = 1024 * 1024;
/// How long an index stays valid after it is generated. A replayed older index
/// is refused once this has passed.
pub const INDEX_VALIDITY_SECS: u64 = 3 * 24 * 60 * 60;
/// How far an index's own times may differ from the puller's clock.
pub const INDEX_CLOCK_SKEW_SECS: u64 = 600;

/// Which distribution lane an entry belongs to. The publication lane's trust is
/// the pinned publisher key plus the pinned node key, with no release
/// evidence chain; the native lane goes through the release node's evidence
/// and a curated set. The two never mix in one install plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexLane {
    Publication,
    Native,
}

/// A bundle one entry requires.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndexRequirement {
    pub name: String,
    /// Exact (`1.2.3`) or caret (`^1.2`).
    pub version: String,
}

/// One bundle version a node serves.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndexEntry {
    pub lane: IndexLane,
    /// The publication id.
    pub id: String,
    /// SHA-256 of the canonical record body: what the pull's `--expect` pins.
    pub record_hash: String,
    pub name: String,
    pub version: String,
    /// Fingerprint of the key that signed the bundle, from the frozen manifest.
    pub publisher: String,
    pub requires: Vec<IndexRequirement>,
    /// `<major>.<minor>` of the node build that serves it. A puller on another
    /// series refuses before any fetch.
    pub node_major_minor: String,
    /// RFC 3339 UTC: the record's `not_after`.
    pub not_after: String,
}

/// The signed index body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicationIndexBody {
    pub schema: String,
    pub node_fingerprint: String,
    /// Moves up whenever the set of entries changes.
    pub sequence: u64,
    pub generated_at: String,
    pub valid_until: String,
    /// Sorted, with no two entries for one `(name, version)`.
    pub entries: Vec<IndexEntry>,
}

/// Why an index was not accepted. Every refusal has a stable code.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IndexRefusal {
    #[error("index_malformed: {0}")]
    Malformed(String),
    #[error("index_signature_invalid: the index is not signed by the pinned node key")]
    BadSignature,
    #[error("index_stale: the index is not valid at this time ({0})")]
    Stale(String),
}

impl IndexRefusal {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Malformed(_) => "index_malformed",
            Self::BadSignature => "index_signature_invalid",
            Self::Stale(_) => "index_stale",
        }
    }
}

fn is_name(value: &str) -> bool {
    (1..=64).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

fn is_version(value: &str) -> bool {
    (1..=64).contains(&value.len()) && value.bytes().all(|b| b.is_ascii_graphic())
}

fn is_major_minor(value: &str) -> bool {
    let mut parts = value.split('.');
    matches!(
        (parts.next(), parts.next(), parts.next()),
        (Some(major), Some(minor), None)
            if !major.is_empty()
                && !minor.is_empty()
                && major.bytes().all(|b| b.is_ascii_digit())
                && minor.bytes().all(|b| b.is_ascii_digit())
    )
}

impl PublicationIndexBody {
    /// The canonical JSON the node signs.
    pub fn canonical(&self) -> Result<String, IndexRefusal> {
        serde_json::to_string(self)
            .map_err(|_| IndexRefusal::Malformed("the index is not canonicalizable".into()))
    }

    /// SHA-256 of the canonical body.
    pub fn body_hash(&self) -> Result<String, IndexRefusal> {
        Ok(lillux::sha256_hex(self.canonical()?.as_bytes()))
    }

    /// SHA-256 of the canonical entries alone. A puller's high-water compares
    /// this, not the body hash: the body's own times change on every request,
    /// the entries change only when a publication does.
    pub fn entries_digest(&self) -> Result<String, IndexRefusal> {
        let entries = serde_json::to_string(&self.entries)
            .map_err(|_| IndexRefusal::Malformed("the entries are not canonicalizable".into()))?;
        Ok(lillux::sha256_hex(entries.as_bytes()))
    }

    fn signing_text(&self, body_hash: &str) -> String {
        format!("{INDEX_DOMAIN}\n{}\n{body_hash}", self.node_fingerprint)
    }

    /// Shape checks that need no key and no clock.
    pub fn validate_shape(&self) -> Result<(), IndexRefusal> {
        let malformed = |text: &str| Err(IndexRefusal::Malformed(text.to_owned()));
        if self.schema != PUBLICATION_INDEX_SCHEMA {
            return malformed("unknown schema");
        }
        if !is_publication_hash(&self.node_fingerprint) {
            return malformed("node_fingerprint is not a SHA-256 fingerprint");
        }
        if self.entries.len() > MAX_INDEX_ENTRIES {
            return malformed("too many entries");
        }
        let mut previous: Option<(&str, &str)> = None;
        for entry in &self.entries {
            if !is_publication_id(&entry.id) {
                return malformed("an entry id is not 32 lowercase hex digits");
            }
            if !is_publication_hash(&entry.record_hash) {
                return malformed("an entry record_hash is not a lowercase SHA-256 digest");
            }
            if !is_publication_hash(&entry.publisher) {
                return malformed("an entry publisher is not a SHA-256 fingerprint");
            }
            if !is_name(&entry.name) || !is_version(&entry.version) {
                return malformed("an entry name or version is not canonical");
            }
            if !is_major_minor(&entry.node_major_minor) {
                return malformed("an entry node_major_minor is not `<major>.<minor>`");
            }
            if entry.requires.len() > MAX_INDEX_REQUIRES {
                return malformed("an entry has too many requirements");
            }
            for requirement in &entry.requires {
                if !is_name(&requirement.name) || !is_version(&requirement.version) {
                    return malformed("a requirement name or version is not canonical");
                }
            }
            if lillux::time::unix_secs_from_iso8601(&entry.not_after).is_none() {
                return malformed("an entry not_after is not a canonical UTC instant");
            }
            let key = (entry.name.as_str(), entry.version.as_str());
            if let Some(before) = previous
                && before >= key
            {
                return malformed("entries are not sorted by (name, version) without repeats");
            }
            previous = Some(key);
        }
        let (generated, valid_until) = self.times()?;
        if valid_until <= generated {
            return malformed("valid_until is not after generated_at");
        }
        Ok(())
    }

    /// `generated_at` and `valid_until` as seconds since the Unix epoch.
    pub fn times(&self) -> Result<(u64, u64), IndexRefusal> {
        let generated =
            lillux::time::unix_secs_from_iso8601(&self.generated_at).ok_or_else(|| {
                IndexRefusal::Malformed("generated_at is not a canonical instant".into())
            })?;
        let valid_until =
            lillux::time::unix_secs_from_iso8601(&self.valid_until).ok_or_else(|| {
                IndexRefusal::Malformed("valid_until is not a canonical instant".into())
            })?;
        Ok((generated, valid_until))
    }
}

/// The form a stranger is served: the body, its hash and the node signature.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServedIndex {
    pub body: PublicationIndexBody,
    pub body_hash: String,
    pub node_signature: String,
}

impl ServedIndex {
    /// Sign a body with the node key.
    pub fn sign(body: PublicationIndexBody, node_key: &SigningKey) -> Result<Self, IndexRefusal> {
        body.validate_shape()?;
        let body_hash = body.body_hash()?;
        let signature = node_key.sign(body.signing_text(&body_hash).as_bytes());
        Ok(Self {
            node_signature: base64::engine::general_purpose::STANDARD.encode(signature.to_bytes()),
            body_hash,
            body,
        })
    }

    /// What a puller checks: the shape, the pinned node's fingerprint, the
    /// body hash, the node signature, and that the index is valid at `now_secs`
    /// within the stated skew.
    pub fn verify(&self, node_key: &VerifyingKey, now_secs: u64) -> Result<(), IndexRefusal> {
        self.body.validate_shape()?;
        if self.body.node_fingerprint != lillux::signature::compute_fingerprint(node_key) {
            return Err(IndexRefusal::BadSignature);
        }
        if self.body.body_hash()? != self.body_hash {
            return Err(IndexRefusal::BadSignature);
        }
        if !lillux::signature::verify_signature(
            &self.body.signing_text(&self.body_hash),
            &self.node_signature,
            node_key,
        ) {
            return Err(IndexRefusal::BadSignature);
        }
        let (generated, valid_until) = self.body.times()?;
        if generated > now_secs.saturating_add(INDEX_CLOCK_SKEW_SECS) {
            return Err(IndexRefusal::Stale("it is dated in the future".into()));
        }
        if valid_until.saturating_add(INDEX_CLOCK_SKEW_SECS) < now_secs {
            return Err(IndexRefusal::Stale(
                "its validity has passed; ask the registry for a current one".into(),
            ));
        }
        Ok(())
    }

    /// Resolve `name@version` to its one entry. A name that is not listed, or a
    /// version that is not listed under it, is a typed refusal that names the
    /// versions the index does hold.
    pub fn resolve(&self, name: &str, version: &str) -> Result<&IndexEntry, ResolveRefusal> {
        let versions: Vec<&str> = self
            .body
            .entries
            .iter()
            .filter(|entry| entry.name == name)
            .map(|entry| entry.version.as_str())
            .collect();
        if versions.is_empty() {
            return Err(ResolveRefusal::UnknownName(name.to_owned()));
        }
        self.body
            .entries
            .iter()
            .find(|entry| entry.name == name && entry.version == version)
            .ok_or_else(|| ResolveRefusal::UnknownVersion {
                name: name.to_owned(),
                version: version.to_owned(),
                available: versions.iter().map(|v| (*v).to_owned()).collect(),
            })
    }
}

/// Why `name@version` did not resolve.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ResolveRefusal {
    #[error("index_unknown_name: the registry lists no bundle named `{0}`")]
    UnknownName(String),
    #[error(
        "index_unknown_version: the registry lists no `{name}@{version}`; it lists {}",
        available.join(", ")
    )]
    UnknownVersion {
        name: String,
        version: String,
        available: Vec<String>,
    },
}

impl ResolveRefusal {
    pub fn code(&self) -> &'static str {
        match self {
            Self::UnknownName(_) => "index_unknown_name",
            Self::UnknownVersion { .. } => "index_unknown_version",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_800_000_000;

    fn node() -> SigningKey {
        SigningKey::from_bytes(&[9; 32])
    }

    fn fingerprint(key: &SigningKey) -> String {
        lillux::signature::compute_fingerprint(&key.verifying_key())
    }

    fn entry(name: &str, version: &str, byte: u8) -> IndexEntry {
        IndexEntry {
            lane: IndexLane::Publication,
            id: format!("{byte:02x}").repeat(16),
            record_hash: format!("{byte:02x}").repeat(32),
            name: name.to_owned(),
            version: version.to_owned(),
            publisher: "ab".repeat(32),
            requires: Vec::new(),
            node_major_minor: "0.5".to_owned(),
            not_after: lillux::time::iso8601_from_unix_secs(NOW + 86_400),
        }
    }

    fn body(entries: Vec<IndexEntry>) -> PublicationIndexBody {
        PublicationIndexBody {
            schema: PUBLICATION_INDEX_SCHEMA.to_owned(),
            node_fingerprint: fingerprint(&node()),
            sequence: 4,
            generated_at: lillux::time::iso8601_from_unix_secs(NOW),
            valid_until: lillux::time::iso8601_from_unix_secs(NOW + INDEX_VALIDITY_SECS),
            entries,
        }
    }

    fn served() -> ServedIndex {
        ServedIndex::sign(
            body(vec![
                entry("demo", "1.0.0", 1),
                entry("demo", "1.1.0", 2),
                entry("tool", "0.2.0", 3),
            ]),
            &node(),
        )
        .unwrap()
    }

    #[test]
    fn a_signed_index_verifies_under_the_pinned_node_key_and_not_under_another() {
        let index = served();
        index.verify(&node().verifying_key(), NOW + 10).unwrap();
        let other = SigningKey::from_bytes(&[8; 32]);
        assert_eq!(
            index.verify(&other.verifying_key(), NOW + 10).unwrap_err(),
            IndexRefusal::BadSignature
        );
    }

    #[test]
    fn an_edited_index_no_longer_verifies() {
        let mut index = served();
        index.body.entries[0].version = "9.9.9".to_owned();
        assert!(index.verify(&node().verifying_key(), NOW + 10).is_err());
    }

    #[test]
    fn a_replayed_old_index_is_refused_once_its_validity_has_passed() {
        let index = served();
        let key = node().verifying_key();
        index.verify(&key, NOW + INDEX_VALIDITY_SECS).unwrap();
        // Past validity plus the stated skew.
        let late = NOW + INDEX_VALIDITY_SECS + INDEX_CLOCK_SKEW_SECS + 1;
        assert_eq!(index.verify(&key, late).unwrap_err().code(), "index_stale");
        // An index dated in the future is refused too.
        assert_eq!(
            index
                .verify(&key, NOW - INDEX_CLOCK_SKEW_SECS - 1)
                .unwrap_err()
                .code(),
            "index_stale"
        );
    }

    #[test]
    fn the_shape_refuses_unsorted_repeated_and_oversize_entries() {
        let unsorted = body(vec![entry("tool", "0.2.0", 3), entry("demo", "1.0.0", 1)]);
        assert!(unsorted.validate_shape().is_err());
        let repeated = body(vec![entry("demo", "1.0.0", 1), entry("demo", "1.0.0", 2)]);
        assert!(repeated.validate_shape().is_err());
        let mut requires = entry("demo", "1.0.0", 1);
        requires.requires = (0..=MAX_INDEX_REQUIRES)
            .map(|n| IndexRequirement {
                name: format!("dep{n}"),
                version: "1.0.0".to_owned(),
            })
            .collect();
        assert!(body(vec![requires]).validate_shape().is_err());
        let many: Vec<IndexEntry> = (0..=MAX_INDEX_ENTRIES)
            .map(|n| entry(&format!("b{n:04}"), "1.0.0", (n % 200) as u8))
            .collect();
        assert!(body(many).validate_shape().is_err());
    }

    #[test]
    fn name_and_version_resolve_to_one_entry_or_a_typed_refusal_naming_what_exists() {
        let index = served();
        assert_eq!(index.resolve("demo", "1.1.0").unwrap().id, "02".repeat(16));
        let unknown = index.resolve("nothing", "1.0.0").unwrap_err();
        assert_eq!(unknown.code(), "index_unknown_name");
        let version = index.resolve("demo", "2.0.0").unwrap_err();
        assert_eq!(version.code(), "index_unknown_version");
        assert!(version.to_string().contains("1.0.0, 1.1.0"), "{version}");
    }
}
