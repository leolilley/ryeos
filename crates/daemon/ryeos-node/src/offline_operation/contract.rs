//! Schemas of the signed offline-operation request, the node-signed receipt
//! and the node-signed refusal, with the digests and bounds that go with them.
//!
//! A request is a canonical JSON body signed with Ed25519, in the same family
//! as the capacity re-issuance and re-anchor documents: the document is
//! `{ "body": …, "signature": "ed25519:<base64>" }` and the signature covers
//! the canonical JSON of `body`. A request's digest is the SHA-256 of that
//! canonical body, so re-encoding the document cannot change its identity.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail, ensure};
use base64::Engine as _;
use chrono::{DateTime, Duration, Utc};
use lillux::crypto::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const REQUEST_SCHEMA: &str = "ryeos/offline-operation-request/v1";
pub const RECEIPT_SCHEMA: &str = "ryeos/offline-operation-receipt/v1";
pub const REFUSAL_SCHEMA: &str = "ryeos/offline-operation-refusal/v1";
pub const CHECKPOINT_SCHEMA: &str = "ryeos/offline-operation-ledger-checkpoint/v1";

/// A request document larger than this is refused unread.
pub const MAX_REQUEST_BYTES: u64 = 64 * 1024;
/// A receipt embeds its request, so it is bounded a little above one.
pub const MAX_RECEIPT_BYTES: u64 = 192 * 1024;
/// The inbox holds at most this many requests; the rest wait outside it.
pub const MAX_INBOX_REQUESTS: usize = 16;
/// Longest a signer may let a request live.
pub const MAX_REQUEST_LIFETIME: Duration = Duration::hours(24);
/// How long a request lives when its signer names no other lifetime.
pub const DEFAULT_REQUEST_LIFETIME: Duration = Duration::hours(1);
/// How far ahead of this node's clock a request's `issued_at` may be before
/// it counts as being from the future. Signers and nodes are different hosts.
pub const CLOCK_SKEW_ALLOWANCE: Duration = Duration::minutes(2);
/// Most arguments bytes a request may carry; the request bound already caps
/// the whole document, this keeps one field from using all of it.
pub const MAX_ARGUMENT_BYTES: usize = 32 * 1024;

/// Whether `value` is a lowercase hexadecimal SHA-256 digest.
pub fn is_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// The ledger head of a node with no receipts. Binding the node's fingerprint
/// makes a head signed for one node meaningless on another.
pub fn genesis_ledger_head(node_fingerprint: &str) -> String {
    lillux::sha256_hex(
        format!("ryeos/offline-operation-ledger-genesis/v1:{node_fingerprint}").as_bytes(),
    )
}

/// The canonical JSON bytes a signature covers.
pub fn canonical_bytes<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    Ok(lillux::canonical_json(&serde_json::to_value(value)?)?.into_bytes())
}

pub fn digest_of<T: Serialize>(value: &T) -> Result<String> {
    Ok(lillux::sha256_hex(&canonical_bytes(value)?))
}

pub fn encode_signature(signature: &Signature) -> String {
    format!(
        "ed25519:{}",
        base64::engine::general_purpose::STANDARD.encode(signature.to_bytes())
    )
}

pub fn decode_signature(encoded: &str) -> Result<Signature> {
    let encoded = encoded
        .strip_prefix("ed25519:")
        .context("signature is not `ed25519:<base64>`")?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .context("signature is not base64")?;
    Signature::from_slice(&bytes).context("signature is not an Ed25519 signature")
}

/// Parse an RFC 3339 instant.
pub fn parse_instant(field: &str, value: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .map(|instant| instant.with_timezone(&Utc))
        .with_context(|| format!("`{field}` is not an RFC 3339 instant"))
}

/// The body of a request: everything the signer commits to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestBody {
    pub schema: String,
    /// The node this request is for.
    pub node_fingerprint: String,
    pub signer_fingerprint: String,
    /// Digest of the signer's grant file as it stood at signing time. Present
    /// for a remote operator, whose authority is that grant; absent for the
    /// node's local operator, whose authority is the key on the node's volume
    /// and who must be able to act when every grant is broken.
    pub signer_grant_digest: Option<String>,
    /// The origin site the signer's grant is bound to. Present exactly when
    /// the grant digest is.
    pub signer_origin_site_id: Option<String>,
    pub operation: String,
    pub arguments: Value,
    /// Digest of each input slot's bytes.
    pub inputs: BTreeMap<String, String>,
    /// Digests of what the operation must produce; the handler refuses
    /// before committing when its result differs.
    pub expected_result: BTreeMap<String, String>,
    /// The node facts this request was signed against. `init_completion` and
    /// `ledger_head` are always present.
    pub precondition: BTreeMap<String, String>,
    pub issued_at: String,
    pub not_after: String,
}

impl RequestBody {
    /// Structural checks that need no knowledge of the node.
    pub fn validate_shape(&self) -> Result<()> {
        ensure!(
            self.schema == REQUEST_SCHEMA,
            "request schema is not {REQUEST_SCHEMA}"
        );
        ensure!(
            is_digest(&self.node_fingerprint) && is_digest(&self.signer_fingerprint),
            "request fingerprints are not SHA-256 digests"
        );
        match (&self.signer_grant_digest, &self.signer_origin_site_id) {
            (Some(digest), Some(site)) => {
                ensure!(is_digest(digest), "signer grant digest is not a digest");
                ryeos_node_runtime::identity::validate_canonical_site_id(site)
                    .context("signer origin site id is not canonical")?;
            }
            (None, None) => {}
            _ => bail!("signer grant digest and origin site are given together or not at all"),
        }
        ensure!(
            self.arguments.is_object(),
            "request arguments are not an object"
        );
        ensure!(
            serde_json::to_vec(&self.arguments)?.len() <= MAX_ARGUMENT_BYTES,
            "request arguments exceed {MAX_ARGUMENT_BYTES} bytes"
        );
        for (slot, digest) in &self.inputs {
            ensure!(is_token(slot), "input slot `{slot}` is not a plain name");
            ensure!(is_digest(digest), "input `{slot}` digest is not a digest");
        }
        for (fact, digest) in self.expected_result.iter().chain(&self.precondition) {
            ensure!(is_token(fact), "fact `{fact}` is not a plain name");
            ensure!(
                is_digest(digest) || is_fact_marker(digest),
                "fact `{fact}` is neither a digest nor a recognised marker"
            );
        }
        Ok(())
    }

    pub fn issued_at(&self) -> Result<DateTime<Utc>> {
        parse_instant("issued_at", &self.issued_at)
    }

    pub fn not_after(&self) -> Result<DateTime<Utc>> {
        parse_instant("not_after", &self.not_after)
    }

    /// The lifetime window must be positive and within the maximum.
    pub fn validate_lifetime(&self) -> Result<()> {
        let issued = self.issued_at()?;
        let not_after = self.not_after()?;
        ensure!(not_after > issued, "`not_after` is not after `issued_at`");
        ensure!(
            not_after - issued <= MAX_REQUEST_LIFETIME,
            "request lifetime exceeds {} hours",
            MAX_REQUEST_LIFETIME.num_hours()
        );
        Ok(())
    }
}

/// A fact value that is not a digest: the thing it measures is not there, or
/// could not be read.
pub const FACT_ABSENT: &str = "absent";
pub const FACT_UNREADABLE: &str = "unreadable";

fn is_fact_marker(value: &str) -> bool {
    value == FACT_ABSENT || value == FACT_UNREADABLE
}

/// Slot and fact names: short lowercase words.
pub fn is_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedRequest {
    pub body: RequestBody,
    pub signature: String,
}

impl SignedRequest {
    pub fn sign(body: RequestBody, key: &SigningKey) -> Result<Self> {
        let signature = key.sign(&canonical_bytes(&body)?);
        Ok(Self {
            body,
            signature: encode_signature(&signature),
        })
    }

    /// The request's identity: the digest of its canonical body.
    pub fn digest(&self) -> Result<String> {
        digest_of(&self.body)
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        let bytes = serde_json::to_vec(self)?;
        ensure!(
            u64::try_from(bytes.len())? <= MAX_REQUEST_BYTES,
            "request document exceeds {MAX_REQUEST_BYTES} bytes"
        );
        Ok(bytes)
    }

    /// Parse request bytes already read within the bound.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        ensure!(
            u64::try_from(bytes.len())? <= MAX_REQUEST_BYTES,
            "request document exceeds {MAX_REQUEST_BYTES} bytes"
        );
        let request: Self =
            serde_json::from_slice(bytes).context("request document is not a signed request")?;
        request.body.validate_shape()?;
        Ok(request)
    }

    pub fn verify_signature(&self, key: &VerifyingKey) -> Result<()> {
        key.verify(
            &canonical_bytes(&self.body)?,
            &decode_signature(&self.signature)?,
        )
        .context("request signature does not verify")
    }
}

/// One applied operation, recorded in the ledger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReceiptBody {
    pub schema: String,
    /// Position in the chain, from 1.
    pub sequence: u64,
    /// Digest of the previous receipt's body, or the genesis head.
    pub previous: String,
    pub node_fingerprint: String,
    pub request_digest: String,
    /// The signed request, so the receipt carries the proof of authority it
    /// was applied under.
    pub request: SignedRequest,
    /// The node facts as measured when the request was checked.
    pub measured_preconditions: BTreeMap<String, String>,
    /// What the handler reported. It never contains input bytes.
    pub result: Value,
    /// Digest of every grant an authorization reset discarded, recorded
    /// before the grants were replaced. Empty for every other operation.
    pub discarded_grant_digests: Vec<String>,
    pub recorded_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedReceipt {
    pub body: ReceiptBody,
    /// Signed with the node key at the time of application.
    pub signature: String,
}

impl SignedReceipt {
    pub fn sign(body: ReceiptBody, node_key: &SigningKey) -> Result<Self> {
        let signature = node_key.sign(&canonical_bytes(&body)?);
        let receipt = Self {
            body,
            signature: encode_signature(&signature),
        };
        ensure!(
            u64::try_from(serde_json::to_vec(&receipt)?.len())? <= MAX_RECEIPT_BYTES,
            "receipt exceeds {MAX_RECEIPT_BYTES} bytes"
        );
        Ok(receipt)
    }

    pub fn digest(&self) -> Result<String> {
        digest_of(&self.body)
    }

    pub fn verify(&self, node_key: &VerifyingKey) -> Result<()> {
        node_key
            .verify(
                &canonical_bytes(&self.body)?,
                &decode_signature(&self.signature)?,
            )
            .context("receipt signature does not verify")
    }
}

/// Carries the chain tip forward over receipts that rotation has dropped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointBody {
    pub schema: String,
    pub node_fingerprint: String,
    /// Sequence of the last receipt the checkpoint stands for.
    pub sequence: u64,
    /// Digest of that receipt's body.
    pub tip: String,
    /// Receipts dropped over the node's lifetime, this checkpoint included.
    pub dropped_receipts: u64,
    pub recorded_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedCheckpoint {
    pub body: CheckpointBody,
    pub signature: String,
}

impl SignedCheckpoint {
    pub fn sign(body: CheckpointBody, node_key: &SigningKey) -> Result<Self> {
        let signature = node_key.sign(&canonical_bytes(&body)?);
        Ok(Self {
            body,
            signature: encode_signature(&signature),
        })
    }

    pub fn verify(&self, node_key: &VerifyingKey) -> Result<()> {
        node_key
            .verify(
                &canonical_bytes(&self.body)?,
                &decode_signature(&self.signature)?,
            )
            .context("checkpoint signature does not verify")
    }
}

/// Why a request was refused. A closed set: each reason is a code in the
/// refusal receipt and a test of the application rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefusalReason {
    /// The document is larger than the bound, or not a request at all.
    Malformed,
    /// Written for another node.
    WrongNode,
    /// The signer is neither the local operator nor a granted key.
    UnknownSigner,
    /// The grant of a signer whose class may not sign offline operations.
    SignerClassNotPermitted,
    BadSignature,
    /// The grant on disk is not the one the request was signed under.
    GrantChanged,
    /// The grant names another origin site than the request.
    OriginMismatch,
    UnknownOperation,
    /// The grant lacks the operation's exact scope.
    ScopeNotGranted,
    /// Only the local operator may sign this operation.
    LocalSignerOnly,
    /// Authority-bearing, and the node's signed profile does not delegate it.
    NotDelegable,
    Expired,
    FromTheFuture,
    InvalidLifetime,
    InvalidArguments,
    InvalidInputs,
    /// The request digest is already in the ledger.
    Replayed,
    /// Signed against a ledger head the ledger has since moved past.
    StaleLedgerHead,
    PreconditionFailed,
    InputMissing,
    InputMismatch,
    InputOversized,
    ExpectedResultMismatch,
    HandlerFailed,
    /// The handler failed, or the node died inside it, three startups running.
    Stuck,
    /// The inbox of a running node holds as many requests as it may.
    InboxFull,
}

impl RefusalReason {
    pub fn code(self) -> &'static str {
        match self {
            Self::Malformed => "malformed",
            Self::WrongNode => "wrong_node",
            Self::UnknownSigner => "unknown_signer",
            Self::SignerClassNotPermitted => "signer_class_not_permitted",
            Self::BadSignature => "bad_signature",
            Self::GrantChanged => "grant_changed",
            Self::OriginMismatch => "origin_mismatch",
            Self::UnknownOperation => "unknown_operation",
            Self::ScopeNotGranted => "scope_not_granted",
            Self::LocalSignerOnly => "local_signer_only",
            Self::NotDelegable => "not_delegable",
            Self::Expired => "expired",
            Self::FromTheFuture => "from_the_future",
            Self::InvalidLifetime => "invalid_lifetime",
            Self::InvalidArguments => "invalid_arguments",
            Self::InvalidInputs => "invalid_inputs",
            Self::Replayed => "replayed",
            Self::StaleLedgerHead => "stale_ledger_head",
            Self::PreconditionFailed => "precondition_failed",
            Self::InputMissing => "input_missing",
            Self::InputMismatch => "input_mismatch",
            Self::InputOversized => "input_oversized",
            Self::ExpectedResultMismatch => "expected_result_mismatch",
            Self::HandlerFailed => "handler_failed",
            Self::Stuck => "stuck",
            Self::InboxFull => "inbox_full",
        }
    }
}

/// A refusal: the reason and the specifics. Never carries input bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub reason: RefusalReason,
    pub detail: String,
    /// The refusal came from a read that failed, not from what was read. A
    /// validly signed request is not discarded for it: it waits for the next
    /// startup, bounded by its own expiry and the attempt counter.
    pub retryable: bool,
}

impl Refusal {
    pub fn new(reason: RefusalReason, detail: impl Into<String>) -> Self {
        Self {
            reason,
            detail: detail.into(),
            retryable: false,
        }
    }

    pub fn retryable(reason: RefusalReason, detail: impl Into<String>) -> Self {
        Self {
            reason,
            detail: detail.into(),
            retryable: true,
        }
    }
}

/// Most bytes of a handler's result a receipt or intent record carries. A
/// larger result is replaced by its length and digest, so one handler cannot
/// make a receipt too large to write.
pub const MAX_RESULT_BYTES: usize = 32 * 1024;

/// The result as it is recorded: itself when it fits, otherwise a marker
/// naming its size and digest.
pub fn bound_result(result: Value) -> Value {
    match serde_json::to_vec(&result) {
        Ok(bytes) if bytes.len() <= MAX_RESULT_BYTES => result,
        Ok(bytes) => serde_json::json!({
            "result_truncated": true,
            "result_bytes": bytes.len(),
            "result_sha256": lillux::sha256_hex(&bytes),
        }),
        Err(error) => serde_json::json!({
            "result_truncated": true,
            "result_error": error.to_string(),
        }),
    }
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.reason.code(), self.detail)
    }
}

impl std::error::Error for Refusal {}

/// The refusal receipt for one (request, reason). Repeats of the same pair
/// raise `occurrences` instead of adding receipts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RefusalBody {
    pub schema: String,
    pub node_fingerprint: String,
    /// The request's digest when it parsed; otherwise the digest of the
    /// bytes that were refused.
    pub request_digest: String,
    pub operation: Option<String>,
    pub signer_fingerprint: Option<String>,
    pub reason: RefusalReason,
    pub detail: String,
    pub occurrences: u64,
    pub first_seen: String,
    pub last_seen: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedRefusal {
    pub body: RefusalBody,
    pub signature: String,
}

impl SignedRefusal {
    pub fn sign(body: RefusalBody, node_key: &SigningKey) -> Result<Self> {
        let signature = node_key.sign(&canonical_bytes(&body)?);
        Ok(Self {
            body,
            signature: encode_signature(&signature),
        })
    }

    pub fn verify(&self, node_key: &VerifyingKey) -> Result<()> {
        node_key
            .verify(
                &canonical_bytes(&self.body)?,
                &decode_signature(&self.signature)?,
            )
            .context("refusal signature does not verify")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body() -> RequestBody {
        RequestBody {
            schema: REQUEST_SCHEMA.to_owned(),
            node_fingerprint: "a".repeat(64),
            signer_fingerprint: "b".repeat(64),
            signer_grant_digest: None,
            signer_origin_site_id: None,
            operation: "node.allow-restart".to_owned(),
            arguments: serde_json::json!({}),
            inputs: BTreeMap::new(),
            expected_result: BTreeMap::new(),
            precondition: BTreeMap::new(),
            issued_at: "2026-10-28T00:00:00Z".to_owned(),
            not_after: "2026-10-28T01:00:00Z".to_owned(),
        }
    }

    #[test]
    fn digest_is_stable_under_reencoding() {
        let key = lillux::crypto::generate_signing_key();
        let request = SignedRequest::sign(body(), &key).unwrap();
        let pretty = serde_json::to_vec_pretty(&request).unwrap();
        let reparsed = SignedRequest::parse(&pretty).unwrap();
        assert_eq!(request.digest().unwrap(), reparsed.digest().unwrap());
        reparsed.verify_signature(&key.verifying_key()).unwrap();
    }

    #[test]
    fn a_tampered_body_does_not_verify() {
        let key = lillux::crypto::generate_signing_key();
        let mut request = SignedRequest::sign(body(), &key).unwrap();
        request.body.operation = "node.reset.authorization".to_owned();
        assert!(request.verify_signature(&key.verifying_key()).is_err());
    }

    #[test]
    fn grant_digest_and_origin_come_together() {
        let mut half = body();
        half.signer_grant_digest = Some("c".repeat(64));
        assert!(half.validate_shape().is_err());
        half.signer_origin_site_id = Some("site:origin".to_owned());
        half.validate_shape().unwrap();
    }

    #[test]
    fn lifetime_is_bounded() {
        let mut long = body();
        long.not_after = "2026-10-29T00:00:01Z".to_owned();
        assert!(long.validate_lifetime().is_err());
        long.not_after = "2026-10-29T00:00:00Z".to_owned();
        long.validate_lifetime().unwrap();
        let mut backwards = body();
        backwards.not_after = backwards.issued_at.clone();
        assert!(backwards.validate_lifetime().is_err());
    }

    #[test]
    fn unknown_fields_are_refused() {
        let key = lillux::crypto::generate_signing_key();
        let request = SignedRequest::sign(body(), &key).unwrap();
        let mut value = serde_json::to_value(&request).unwrap();
        value["body"]["extra"] = serde_json::json!(1);
        assert!(SignedRequest::parse(&serde_json::to_vec(&value).unwrap()).is_err());
    }

    #[test]
    fn oversized_documents_are_refused_unparsed() {
        let bytes = vec![b' '; usize::try_from(MAX_REQUEST_BYTES).unwrap() + 1];
        assert!(SignedRequest::parse(&bytes).is_err());
    }

    #[test]
    fn an_oversized_result_is_recorded_by_size_and_digest() {
        let small = serde_json::json!({ "ok": true });
        assert_eq!(bound_result(small.clone()), small);
        let big = serde_json::json!({ "blob": "x".repeat(MAX_RESULT_BYTES) });
        let bounded = bound_result(big);
        assert_eq!(bounded["result_truncated"], true);
        assert!(serde_json::to_vec(&bounded).unwrap().len() < 1024);
    }

    #[test]
    fn genesis_head_binds_the_node() {
        assert_ne!(
            genesis_ledger_head(&"a".repeat(64)),
            genesis_ledger_head(&"b".repeat(64))
        );
    }
}
