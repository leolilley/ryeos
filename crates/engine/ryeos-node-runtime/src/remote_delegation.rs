//! Principal-signed, bounded delegation for remote execution.
//!
//! A principal on node A signs, with its own key, a statement that A may act
//! for it on node B for a bounded time and within bounded scopes. Node A stores
//! the statement (node-signed) and attaches it to the requests it forwards.
//! Node B admits the principal from its own grants and takes the effective
//! authority to be the principal's grant intersected with the delegation, so a
//! compromised A can only act inside what the principal signed.
//!
//! This module owns the canonical body, the signature, the digest, the
//! validation against the expected parties, the scope intersection, and the two
//! node-signed stores: A's per-(principal, remote) record and B's ledger of
//! revoked delegation digests.

use std::path::Path;

use anyhow::{Context, Result, bail};
use base64::Engine;
use lillux::crypto::{Signature, Signer as _, SigningKey, Verifier as _, VerifyingKey};
use serde::{Deserialize, Serialize};

use crate::identity::{NodeIdentity, validate_canonical_site_id};

/// Schema tag of the signed delegation body.
pub const DELEGATION_SCHEMA: &str = "ryeos.remote_delegation.v1";

/// Request header carrying the base64 of the signed delegation JSON.
pub const DELEGATION_HEADER: &str = "x-ryeos-delegation";

/// Longest lifetime node A accepts when it stores a delegation.
pub const MAX_DELEGATION_LIFETIME_SECS: u64 = 30 * 24 * 60 * 60;

/// Capability a principal needs on node A to store a delegation.
pub const DELEGATE_CAP: &str = "ryeos.execute.service.remote/delegate";

const SIGNING_DOMAIN: &str = "ryeos-remote-delegation-v1";
const STORE_SCHEMA_VERSION: u32 = 1;
const MAX_RECORD_BYTES: u64 = 64 * 1024;

/// A delegation this node will not honour, with the cure each refusal names.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DelegationError {
    #[error(
        "no delegation from {delegator} for remote `{remote}`; sign one with \
         `ryeos remote delegate --remote {remote}`"
    )]
    Missing { delegator: String, remote: String },
    #[error(
        "the delegation from {delegator} for remote `{remote}` expired at unix time {not_after}; \
         sign a new one with `ryeos remote delegate --remote {remote}`"
    )]
    Expired {
        delegator: String,
        remote: String,
        not_after: u64,
    },
    #[error(
        "the delegation from {delegator} for remote `{remote}` was replaced after this operation \
         began (stored {current}, operation holds {retained}); start the operation again"
    )]
    Replaced {
        delegator: String,
        remote: String,
        current: String,
        retained: String,
    },
    #[error(
        "a delegated request cannot itself delegate onward; run the operation from the node that \
         holds the principal's own delegation"
    )]
    NotTransitive,
    #[error("the delegation is malformed: {0}")]
    Malformed(String),
    #[error("the delegation signature does not verify under the delegator's key")]
    BadSignature,
    #[error(
        "the delegation is addressed to {found}, not to this node ({expected}); sign one for this \
         remote with `ryeos remote delegate`"
    )]
    WrongAudience { expected: String, found: String },
    #[error(
        "the delegation names {found} as its delegate, but the request came from {expected}; sign \
         one for the forwarding node with `ryeos remote delegate`"
    )]
    WrongDelegate { expected: String, found: String },
    #[error(
        "the delegation {digest} was revoked on this node; sign a new one with \
         `ryeos remote delegate`"
    )]
    Revoked { digest: String },
    #[error(
        "the delegator {delegator} is not admitted on this node as a remote_principal for the \
         forwarding site; ask this node's operator to admit it"
    )]
    PrincipalNotAdmitted { delegator: String },
}

impl DelegationError {
    /// Stable machine-readable code of the refusal.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Missing { .. } => "remote_delegation_missing",
            Self::Expired { .. } => "remote_delegation_expired",
            Self::Replaced { .. } => "remote_delegation_replaced",
            Self::NotTransitive => "remote_delegation_not_transitive",
            Self::Malformed(_) => "remote_delegation_malformed",
            Self::BadSignature => "remote_delegation_bad_signature",
            Self::WrongAudience { .. } => "remote_delegation_wrong_audience",
            Self::WrongDelegate { .. } => "remote_delegation_wrong_delegate",
            Self::Revoked { .. } => "remote_delegation_revoked",
            Self::PrincipalNotAdmitted { .. } => "remote_delegation_principal_not_admitted",
        }
    }
}

/// The signed statement. Field order is the canonical order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DelegationBody {
    pub schema: String,
    /// `fp:<fingerprint>` of the principal that signs the delegation.
    pub delegator: String,
    /// Site of the node that may act for the delegator.
    pub delegate_site: String,
    /// `fp:<fingerprint>` of that node's key, which signs the forwarded requests.
    pub delegate_node: String,
    /// Site of the node the delegation is addressed to.
    pub audience_site: String,
    /// `fp:<fingerprint>` of the addressed node's key.
    pub audience_node: String,
    /// Capability patterns the delegator lends, sorted and without duplicates.
    pub scopes: Vec<String>,
    /// Last unix second at which the delegation is honoured.
    pub not_after: u64,
    /// Random 32 hex characters that make every delegation distinct.
    pub nonce: String,
}

/// The parties a delegation is checked against.
#[derive(Debug, Clone, Copy)]
pub struct DelegationParties<'a> {
    pub delegate_site: &'a str,
    pub delegate_node: &'a str,
    pub audience_site: &'a str,
    pub audience_node: &'a str,
}

impl DelegationBody {
    /// A new body with canonical scopes and a fresh nonce.
    pub fn new(
        delegator: &str,
        parties: DelegationParties<'_>,
        scopes: &[String],
        not_after: u64,
    ) -> Self {
        let mut scopes = scopes.to_vec();
        scopes.sort();
        scopes.dedup();
        let nonce: [u8; 16] = rand::Rng::r#gen(&mut rand::thread_rng());
        Self {
            schema: DELEGATION_SCHEMA.to_owned(),
            delegator: delegator.to_owned(),
            delegate_site: parties.delegate_site.to_owned(),
            delegate_node: parties.delegate_node.to_owned(),
            audience_site: parties.audience_site.to_owned(),
            audience_node: parties.audience_node.to_owned(),
            scopes,
            not_after,
            nonce: nonce.iter().map(|byte| format!("{byte:02x}")).collect(),
        }
    }

    fn signing_string(&self) -> Result<String> {
        let body = serde_json::to_string(self).context("serialize delegation body")?;
        Ok(format!("{SIGNING_DOMAIN}\n{body}"))
    }

    /// The bytes the delegator signs: the hex digest of the signing string.
    pub fn signing_bytes(&self) -> Result<Vec<u8>> {
        Ok(self.digest()?.into_bytes())
    }

    /// Content digest of the delegation. It names the delegation in request
    /// facts, durable records and the revocation ledger.
    pub fn digest(&self) -> Result<String> {
        Ok(lillux::sha256_hex(self.signing_string()?.as_bytes()))
    }

    /// The delegator's fingerprint without the `fp:` prefix.
    pub fn delegator_fingerprint(&self) -> Result<&str, DelegationError> {
        fingerprint_of(&self.delegator).ok_or_else(|| {
            DelegationError::Malformed("delegator is not a canonical `fp:` principal".to_owned())
        })
    }

    /// Whether the delegation lends a capability that covers `required`.
    pub fn covers(&self, required: &str) -> bool {
        self.scopes
            .iter()
            .any(|scope| ryeos_state::capability::grant_matches(scope, required))
    }

    fn validate_shape(&self) -> Result<(), DelegationError> {
        let malformed = |reason: &str| Err(DelegationError::Malformed(reason.to_owned()));
        if self.schema != DELEGATION_SCHEMA {
            return malformed("unknown schema");
        }
        for (label, value) in [
            ("delegator", &self.delegator),
            ("delegate_node", &self.delegate_node),
            ("audience_node", &self.audience_node),
        ] {
            if fingerprint_of(value).is_none() {
                return Err(DelegationError::Malformed(format!(
                    "{label} is not a canonical `fp:` principal"
                )));
            }
        }
        for (label, value) in [
            ("delegate_site", &self.delegate_site),
            ("audience_site", &self.audience_site),
        ] {
            if validate_canonical_site_id(value).is_err() {
                return Err(DelegationError::Malformed(format!(
                    "{label} is not a canonical site id"
                )));
            }
        }
        if self.scopes.is_empty() {
            return malformed("no scopes");
        }
        if self.scopes.windows(2).any(|pair| pair[0] >= pair[1]) {
            return malformed("scopes are not sorted and unique");
        }
        for scope in &self.scopes {
            if let Err(reason) = ryeos_runtime::authorizer::validate_scope_pattern(scope) {
                return Err(DelegationError::Malformed(format!(
                    "scope `{scope}` is invalid: {reason}"
                )));
            }
        }
        if self.nonce.len() != 32 || !self.nonce.bytes().all(|b| b.is_ascii_hexdigit()) {
            return malformed("nonce is not 32 hex characters");
        }
        Ok(())
    }
}

fn fingerprint_of(principal: &str) -> Option<&str> {
    principal
        .strip_prefix("fp:")
        .filter(|fingerprint| lillux::valid_hash(fingerprint))
}

/// A delegation with the delegator's signature.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedDelegation {
    pub body: DelegationBody,
    /// Base64 Ed25519 signature over [`DelegationBody::signing_bytes`].
    pub signature: String,
}

impl SignedDelegation {
    /// Sign a body with the delegator's own key.
    pub fn sign(body: DelegationBody, delegator_key: &SigningKey) -> Result<Self> {
        let signature = delegator_key.sign(&body.signing_bytes()?);
        Ok(Self {
            body,
            signature: base64::engine::general_purpose::STANDARD.encode(signature.to_bytes()),
        })
    }

    /// Attach a signature computed elsewhere over [`DelegationBody::signing_bytes`].
    pub fn with_signature(body: DelegationBody, signature_b64: String) -> Self {
        Self {
            body,
            signature: signature_b64,
        }
    }

    pub fn digest(&self) -> Result<String> {
        self.body.digest()
    }

    /// Verify the signature under the delegator's key. The key must be the one
    /// the delegator's fingerprint names.
    pub fn verify(&self, delegator_key: &VerifyingKey) -> Result<(), DelegationError> {
        self.body.validate_shape()?;
        let fingerprint = self.body.delegator_fingerprint()?;
        if lillux::signature::compute_fingerprint(delegator_key) != fingerprint {
            return Err(DelegationError::BadSignature);
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&self.signature)
            .map_err(|_| DelegationError::BadSignature)?;
        let signature = Signature::from_slice(&bytes).map_err(|_| DelegationError::BadSignature)?;
        let signing_bytes = self
            .body
            .signing_bytes()
            .map_err(|error| DelegationError::Malformed(format!("{error:#}")))?;
        delegator_key
            .verify(&signing_bytes, &signature)
            .map_err(|_| DelegationError::BadSignature)
    }

    /// Check the parties and the lifetime at `now`.
    pub fn validate_at(
        &self,
        now: u64,
        parties: DelegationParties<'_>,
    ) -> Result<(), DelegationError> {
        self.body.validate_shape()?;
        if self.body.audience_site != parties.audience_site
            || self.body.audience_node != parties.audience_node
        {
            return Err(DelegationError::WrongAudience {
                expected: format!("{} {}", parties.audience_site, parties.audience_node),
                found: format!("{} {}", self.body.audience_site, self.body.audience_node),
            });
        }
        if self.body.delegate_site != parties.delegate_site
            || self.body.delegate_node != parties.delegate_node
        {
            return Err(DelegationError::WrongDelegate {
                expected: format!("{} {}", parties.delegate_site, parties.delegate_node),
                found: format!("{} {}", self.body.delegate_site, self.body.delegate_node),
            });
        }
        if now > self.body.not_after {
            return Err(DelegationError::Expired {
                delegator: self.body.delegator.clone(),
                remote: self.body.audience_site.clone(),
                not_after: self.body.not_after,
            });
        }
        Ok(())
    }

    /// Header value: base64 of the JSON form.
    pub fn to_header_value(&self) -> Result<String> {
        Ok(base64::engine::general_purpose::STANDARD.encode(serde_json::to_vec(self)?))
    }

    /// Parse a header value produced by [`Self::to_header_value`].
    pub fn from_header_value(value: &str) -> Result<Self, DelegationError> {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(value)
            .map_err(|_| DelegationError::Malformed("header is not base64".to_owned()))?;
        if bytes.len() as u64 > MAX_RECORD_BYTES {
            return Err(DelegationError::Malformed("header is too large".to_owned()));
        }
        serde_json::from_slice(&bytes).map_err(|error| {
            DelegationError::Malformed(format!("header is not a delegation: {error}"))
        })
    }
}

/// The authority a delegated request carries: what the delegator's grant on
/// the audience node allows, narrowed to what the delegation lends. A pattern
/// survives when a pattern on the other side covers it, so a delegation of
/// `ryeos.execute.*` against a grant of `ryeos.execute.service.remote/*`
/// yields the narrower grant pattern.
pub fn intersect_scopes(grant: &[String], delegation: &[String]) -> Vec<String> {
    use ryeos_state::capability::pattern_covers;
    let mut effective: Vec<String> = Vec::new();
    for lent in delegation {
        if grant.iter().any(|held| pattern_covers(held, lent)) {
            effective.push(lent.clone());
        }
    }
    for held in grant {
        if delegation.iter().any(|lent| pattern_covers(lent, held)) {
            effective.push(held.clone());
        }
    }
    effective.sort();
    effective.dedup();
    effective
}

// ---------------------------------------------------------------------------
// Node-signed stores
// ---------------------------------------------------------------------------

fn validate_remote_name(remote: &str) -> Result<()> {
    let ok = !remote.is_empty()
        && remote.len() <= 64
        && !remote.starts_with('.')
        && remote
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    if !ok {
        bail!("remote name `{remote}` cannot name a delegation record");
    }
    Ok(())
}

pub(crate) fn read_node_signed(
    directory: &lillux::PinnedDirectory,
    name: &str,
    node: &NodeIdentity,
) -> Result<Option<String>> {
    let Some(file) = directory.open_regular(std::ffi::OsStr::new(name), false)? else {
        directory.ensure_path_binding()?;
        return Ok(None);
    };
    let observation = lillux::observe_open_regular_file(&file)?;
    let bytes =
        lillux::read_open_regular_file_stable_bounded(&file, &observation, MAX_RECORD_BYTES)?;
    directory.ensure_regular_entry_matches(std::ffi::OsStr::new(name), Some(&file))?;
    directory.ensure_path_binding()?;
    let raw = std::str::from_utf8(&bytes).context("record is not UTF-8")?;
    let (body, header) =
        lillux::signature::strip_canonical_signature_with_envelope(raw, "#", None, false)?;
    let header = header.ok_or_else(|| anyhow::anyhow!("unsigned record {name}"))?;
    if header.signer_fingerprint != node.fingerprint() {
        bail!("record {name} is not signed by this node");
    }
    if lillux::sha256_hex(body.as_bytes()) != header.content_hash {
        bail!("record {name} is tampered");
    }
    let signature = Signature::from_slice(
        &base64::engine::general_purpose::STANDARD
            .decode(&header.signature_b64)
            .context("record signature is not base64")?,
    )?;
    node.verify_hash(&header.content_hash, &signature)?;
    Ok(Some(body))
}

pub(crate) fn write_node_signed(
    directory: &lillux::PinnedDirectory,
    name: &str,
    body: &str,
    node: &NodeIdentity,
) -> Result<()> {
    let signed = lillux::signature::sign_content(body, node.signing_key(), "#", None);
    let _lock = directory.lock_exclusive()?;
    directory.ensure_path_binding()?;
    // Replace the incumbent, if any, as exactly the file that was observed.
    let incumbent = directory.open_pinned_regular(std::ffi::OsStr::new(name), false)?;
    directory
        .replace_pinned_bytes_if_matches_atomic(
            std::ffi::OsStr::new(name),
            incumbent.as_ref(),
            |_| Ok(()),
            signed.as_bytes(),
            0o600,
        )
        .map_err(anyhow::Error::from)
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredDelegationBody {
    schema_version: u32,
    remote: String,
    digest: String,
    stored_at: u64,
    delegation: SignedDelegation,
}

fn principal_dir(
    delegations_dir: &Path,
    fingerprint: &str,
    create: bool,
) -> Result<Option<lillux::PinnedDirectory>> {
    if !lillux::valid_hash(fingerprint) {
        bail!("delegator fingerprint is not canonical");
    }
    if create {
        let root = lillux::PinnedDirectory::open_or_create(delegations_dir)?;
        return Ok(Some(
            root.open_or_create_child(std::ffi::OsStr::new(fingerprint), 0o700)?,
        ));
    }
    lillux::PinnedDirectory::open(&delegations_dir.join(fingerprint))
}

/// Store a delegation node A accepted from `delegator`, replacing any earlier
/// one for the same remote. The delegation's signature must already have been
/// verified against the delegator's grant.
pub fn store_delegation(
    delegations_dir: &Path,
    remote: &str,
    delegation: &SignedDelegation,
    node: &NodeIdentity,
    now: u64,
) -> Result<String> {
    validate_remote_name(remote)?;
    let fingerprint = delegation
        .body
        .delegator_fingerprint()
        .map_err(anyhow::Error::from)?
        .to_owned();
    let digest = delegation.digest()?;
    let body = toml::to_string(&StoredDelegationBody {
        schema_version: STORE_SCHEMA_VERSION,
        remote: remote.to_owned(),
        digest: digest.clone(),
        stored_at: now,
        delegation: delegation.clone(),
    })
    .context("serialize stored delegation")?;
    let directory = principal_dir(delegations_dir, &fingerprint, true)?
        .context("delegation directory was not created")?;
    write_node_signed(&directory, &format!("{remote}.toml"), &body, node)?;
    Ok(digest)
}

/// Load the delegation stored for `(delegator, remote)`, if any.
pub fn load_delegation(
    delegations_dir: &Path,
    delegator_fingerprint: &str,
    remote: &str,
    node: &NodeIdentity,
) -> Result<Option<SignedDelegation>> {
    validate_remote_name(remote)?;
    let Some(directory) = principal_dir(delegations_dir, delegator_fingerprint, false)? else {
        return Ok(None);
    };
    let Some(body) = read_node_signed(&directory, &format!("{remote}.toml"), node)? else {
        return Ok(None);
    };
    let stored: StoredDelegationBody =
        toml::from_str(&body).context("stored delegation is not valid")?;
    if stored.schema_version != STORE_SCHEMA_VERSION
        || stored.remote != remote
        || stored.delegation.digest()? != stored.digest
        || stored.delegation.body.delegator_fingerprint()? != delegator_fingerprint
    {
        bail!("stored delegation for remote `{remote}` is inconsistent");
    }
    Ok(Some(stored.delegation))
}

/// Remove the delegation stored for `(delegator, remote)`. Returns whether one
/// existed.
pub fn remove_delegation(
    delegations_dir: &Path,
    delegator_fingerprint: &str,
    remote: &str,
) -> Result<bool> {
    validate_remote_name(remote)?;
    let Some(directory) = principal_dir(delegations_dir, delegator_fingerprint, false)? else {
        return Ok(false);
    };
    let name = format!("{remote}.toml");
    let _lock = directory.lock_exclusive()?;
    let Some(file) = directory.open_pinned_regular(std::ffi::OsStr::new(&name), false)? else {
        return Ok(false);
    };
    directory.remove_pinned_regular_if_same(&file)?;
    Ok(true)
}

/// One stored delegation as listed to its owner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StoredDelegationSummary {
    pub remote: String,
    pub digest: String,
    pub not_after: u64,
    pub scopes: Vec<String>,
    pub audience_site: String,
    pub audience_node: String,
}

/// List the delegations stored for one delegator.
pub fn list_delegations(
    delegations_dir: &Path,
    delegator_fingerprint: &str,
    node: &NodeIdentity,
) -> Result<Vec<StoredDelegationSummary>> {
    let Some(directory) = principal_dir(delegations_dir, delegator_fingerprint, false)? else {
        return Ok(Vec::new());
    };
    let mut names: Vec<String> = directory
        .entries_no_follow()?
        .into_iter()
        .filter_map(|entry| entry.name.to_str().map(str::to_owned))
        .filter_map(|name| name.strip_suffix(".toml").map(str::to_owned))
        .collect();
    names.sort();
    let mut summaries = Vec::new();
    for remote in names {
        if let Some(delegation) =
            load_delegation(delegations_dir, delegator_fingerprint, &remote, node)?
        {
            summaries.push(StoredDelegationSummary {
                digest: delegation.digest()?,
                not_after: delegation.body.not_after,
                scopes: delegation.body.scopes.clone(),
                audience_site: delegation.body.audience_site.clone(),
                audience_node: delegation.body.audience_node.clone(),
                remote,
            });
        }
    }
    Ok(summaries)
}

/// A revocation tombstone on the audience node, keyed by delegation digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DelegationTombstone {
    pub schema_version: u32,
    pub delegation_digest: String,
    pub delegator: String,
    pub revoked_at: u64,
    /// Principal that asked for the revocation.
    pub revoked_by: String,
    /// Site of the node that forwarded the request, when the principal asked
    /// through a delegation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegated_via: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_digest: Option<String>,
}

/// Record a tombstone for `delegation_digest`. Idempotent: an existing
/// tombstone is kept, since the first revocation is the one that counts.
pub fn write_tombstone(
    revocations_dir: &Path,
    delegation_digest: &str,
    delegator: &str,
    revoked_by: &str,
    delegated_via: Option<String>,
    request_digest: Option<String>,
    node: &NodeIdentity,
    now: u64,
) -> Result<DelegationTombstone> {
    if !lillux::valid_hash(delegation_digest) {
        bail!("delegation digest is not canonical");
    }
    if fingerprint_of(delegator).is_none() {
        bail!("delegator is not a canonical principal");
    }
    let directory = lillux::PinnedDirectory::open_or_create(revocations_dir)?;
    let name = format!("{delegation_digest}.toml");
    if let Some(existing) = read_node_signed(&directory, &name, node)? {
        return toml::from_str(&existing).context("existing tombstone is not valid");
    }
    let tombstone = DelegationTombstone {
        schema_version: STORE_SCHEMA_VERSION,
        delegation_digest: delegation_digest.to_owned(),
        delegator: delegator.to_owned(),
        revoked_at: now,
        revoked_by: revoked_by.to_owned(),
        delegated_via,
        request_digest,
    };
    let body = toml::to_string(&tombstone).context("serialize tombstone")?;
    write_node_signed(&directory, &name, &body, node)?;
    Ok(tombstone)
}

/// Whether the ledger holds a tombstone for `delegation_digest`. A tombstone
/// that cannot be verified is an error, never an absent one.
pub fn is_revoked(
    revocations_dir: &Path,
    delegation_digest: &str,
    node: &NodeIdentity,
) -> Result<bool> {
    if !lillux::valid_hash(delegation_digest) {
        bail!("delegation digest is not canonical");
    }
    let Some(directory) = lillux::PinnedDirectory::open(revocations_dir)? else {
        return Ok(false);
    };
    Ok(read_node_signed(&directory, &format!("{delegation_digest}.toml"), node)?.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lillux::crypto::SigningKey;

    fn key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    fn principal(seed: u8) -> String {
        format!(
            "fp:{}",
            lillux::signature::compute_fingerprint(&key(seed).verifying_key())
        )
    }

    fn site(seed: u8) -> String {
        format!("site:{}", lillux::sha256_hex(&[seed]))
    }

    struct Parties {
        delegate_site: String,
        delegate_node: String,
        audience_site: String,
        audience_node: String,
    }

    impl Parties {
        fn new() -> Self {
            Self {
                delegate_site: site(1),
                delegate_node: principal(11),
                audience_site: site(2),
                audience_node: principal(12),
            }
        }
        fn view(&self) -> DelegationParties<'_> {
            DelegationParties {
                delegate_site: &self.delegate_site,
                delegate_node: &self.delegate_node,
                audience_site: &self.audience_site,
                audience_node: &self.audience_node,
            }
        }
    }

    fn signed(parties: &Parties, not_after: u64) -> SignedDelegation {
        let body = DelegationBody::new(
            &principal(5),
            parties.view(),
            &["ryeos.execute.service.remote/run".to_owned()],
            not_after,
        );
        SignedDelegation::sign(body, &key(5)).unwrap()
    }

    #[test]
    fn delegation_sign_verify_round_trip() {
        let parties = Parties::new();
        let delegation = signed(&parties, 2_000);
        delegation.verify(&key(5).verifying_key()).unwrap();
        delegation.validate_at(1_000, parties.view()).unwrap();
        let header = delegation.to_header_value().unwrap();
        let parsed = SignedDelegation::from_header_value(&header).unwrap();
        assert_eq!(parsed, delegation);
        assert_eq!(parsed.digest().unwrap(), delegation.digest().unwrap());
    }

    #[test]
    fn delegation_refuses_wrong_audience_site() {
        let parties = Parties::new();
        let delegation = signed(&parties, 2_000);
        let other = site(9);
        let mut view = parties.view();
        view.audience_site = &other;
        let error = delegation.validate_at(1_000, view).unwrap_err();
        assert_eq!(error.code(), "remote_delegation_wrong_audience");
    }

    #[test]
    fn delegation_refuses_wrong_delegate_node() {
        let parties = Parties::new();
        let delegation = signed(&parties, 2_000);
        let other = principal(40);
        let mut view = parties.view();
        view.delegate_node = &other;
        let error = delegation.validate_at(1_000, view).unwrap_err();
        assert_eq!(error.code(), "remote_delegation_wrong_delegate");
    }

    #[test]
    fn delegation_refuses_after_not_after() {
        let parties = Parties::new();
        let delegation = signed(&parties, 2_000);
        delegation.validate_at(2_000, parties.view()).unwrap();
        let error = delegation.validate_at(2_001, parties.view()).unwrap_err();
        assert_eq!(error.code(), "remote_delegation_expired");
    }

    #[test]
    fn delegation_scope_must_cover_required_cap() {
        let parties = Parties::new();
        let delegation = signed(&parties, 2_000);
        assert!(delegation.body.covers("ryeos.execute.service.remote/run"));
        assert!(!delegation.body.covers("ryeos.execute.service.remote/push"));
        assert!(
            !delegation
                .body
                .covers("ryeos.execute.service.remote/run/extra")
        );
    }

    #[test]
    fn delegation_tamper_detected() {
        let parties = Parties::new();
        let mut delegation = signed(&parties, 2_000);
        delegation.body.not_after = 9_999_999;
        assert_eq!(
            delegation
                .verify(&key(5).verifying_key())
                .unwrap_err()
                .code(),
            "remote_delegation_bad_signature"
        );
        let mut widened = signed(&parties, 2_000);
        widened.body.scopes = vec!["*".to_owned()];
        assert_eq!(
            widened.verify(&key(5).verifying_key()).unwrap_err().code(),
            "remote_delegation_bad_signature"
        );
        // Another principal's key never verifies it, even with a good signature.
        let delegation = signed(&parties, 2_000);
        assert_eq!(
            delegation
                .verify(&key(6).verifying_key())
                .unwrap_err()
                .code(),
            "remote_delegation_bad_signature"
        );
    }

    #[test]
    fn delegation_refuses_non_canonical_scopes() {
        let parties = Parties::new();
        let mut body =
            DelegationBody::new(&principal(5), parties.view(), &["a.b".to_owned()], 2_000);
        body.scopes = vec!["b.c".to_owned(), "a.b".to_owned()];
        let delegation = SignedDelegation::sign(body, &key(5)).unwrap();
        assert_eq!(
            delegation
                .verify(&key(5).verifying_key())
                .unwrap_err()
                .code(),
            "remote_delegation_malformed"
        );
    }

    #[test]
    fn scope_intersection_keeps_only_what_both_sides_allow() {
        let grant = vec![
            "ryeos.execute.service.remote/*".to_owned(),
            "ryeos.fetch.item.read".to_owned(),
        ];
        let lent = vec![
            "ryeos.execute.service.remote/run".to_owned(),
            "ryeos.execute.service.remote/push".to_owned(),
            "ryeos.execute.tool.*".to_owned(),
        ];
        assert_eq!(
            intersect_scopes(&grant, &lent),
            vec![
                "ryeos.execute.service.remote/push".to_owned(),
                "ryeos.execute.service.remote/run".to_owned(),
            ]
        );
        // A wider delegation never widens the grant.
        assert_eq!(
            intersect_scopes(&grant, &["*".to_owned()]),
            vec![
                "ryeos.execute.service.remote/*".to_owned(),
                "ryeos.fetch.item.read".to_owned(),
            ]
        );
        assert!(intersect_scopes(&grant, &["ryeos.other".to_owned()]).is_empty());
    }

    fn node(dir: &Path) -> NodeIdentity {
        let path = dir.join("node.pem");
        lillux::crypto::create_signing_key(&path).unwrap();
        NodeIdentity::load(&path).unwrap()
    }

    #[test]
    fn stored_delegation_round_trips_and_is_replaced_by_remote_name() {
        let tmp = tempfile::tempdir().unwrap();
        let node = node(tmp.path());
        let dir = tmp.path().join("delegations");
        let parties = Parties::new();
        let first = signed(&parties, 2_000);
        let fingerprint = first.body.delegator_fingerprint().unwrap().to_owned();
        assert!(
            load_delegation(&dir, &fingerprint, "b", &node)
                .unwrap()
                .is_none()
        );
        store_delegation(&dir, "b", &first, &node, 1_000).unwrap();
        assert_eq!(
            load_delegation(&dir, &fingerprint, "b", &node)
                .unwrap()
                .unwrap(),
            first
        );
        let second = signed(&parties, 3_000);
        store_delegation(&dir, "b", &second, &node, 1_001).unwrap();
        assert_eq!(
            load_delegation(&dir, &fingerprint, "b", &node)
                .unwrap()
                .unwrap(),
            second
        );
        assert_eq!(
            list_delegations(&dir, &fingerprint, &node).unwrap().len(),
            1
        );
        assert!(remove_delegation(&dir, &fingerprint, "b").unwrap());
        assert!(!remove_delegation(&dir, &fingerprint, "b").unwrap());
        assert!(
            load_delegation(&dir, &fingerprint, "b", &node)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn stored_delegation_refuses_another_nodes_signature() {
        let tmp = tempfile::tempdir().unwrap();
        let node = node(tmp.path());
        let other_dir = tempfile::tempdir().unwrap();
        let other = self::node(other_dir.path());
        let dir = tmp.path().join("delegations");
        let parties = Parties::new();
        let delegation = signed(&parties, 2_000);
        let fingerprint = delegation.body.delegator_fingerprint().unwrap().to_owned();
        store_delegation(&dir, "b", &delegation, &node, 1_000).unwrap();
        assert!(load_delegation(&dir, &fingerprint, "b", &other).is_err());
    }

    #[test]
    fn tombstone_ledger_marks_one_digest() {
        let tmp = tempfile::tempdir().unwrap();
        let node = node(tmp.path());
        let dir = tmp.path().join("revocations");
        let parties = Parties::new();
        let revoked = signed(&parties, 2_000);
        let other = signed(&parties, 2_000);
        let digest = revoked.digest().unwrap();
        assert!(!is_revoked(&dir, &digest, &node).unwrap());
        write_tombstone(
            &dir,
            &digest,
            &revoked.body.delegator,
            &principal(5),
            None,
            None,
            &node,
            1_000,
        )
        .unwrap();
        // Writing again keeps the first tombstone.
        let again = write_tombstone(
            &dir,
            &digest,
            &revoked.body.delegator,
            &principal(6),
            None,
            None,
            &node,
            2_000,
        )
        .unwrap();
        assert_eq!(again.revoked_at, 1_000);
        assert!(is_revoked(&dir, &digest, &node).unwrap());
        assert!(!is_revoked(&dir, &other.digest().unwrap(), &node).unwrap());
    }
}
