//! The one answer shape every general read returns.
//!
//! ```json
//! { "schema": "ryeos.read.<family>.<name>/v1",
//!   "as_of": "<UTC instant, daemon clock>",
//!   "revision": "<opaque token for the state this answer was read from>",
//!   "coverage": { "state": "complete | observed | partial | failed | unavailable", "reason": "<code or null>", ... },
//!   "rows": [ ... ]            // or "record": { ... } for a single-record read
//!   "page": { "next_cursor": "<opaque or null>", "total": 172, "total_basis": "exact | at_least | unknown" },
//!   "signers": { "fp:<hex>": { "label": "...", "role": "...", "status": "active | retired | revoked | node | unknown" } } }
//! ```
//!
//! A caller that sends `if_revision` equal to the answer's revision gets
//! `{ "not_modified": true, "revision": ... }` and no rows.
//!
//! The module holds no disclosure or class logic. A read authorizes first and
//! then builds its envelope from what the caller may see: `page.total`
//! counts the authorized filter, never the page; the revision is derived from
//! the source tips together with the caller's filter, so a token never tells
//! a caller about state its filter does not reach; `signers` names only the
//! fingerprints in the answer, resolved when the answer is built.
//!
//! The cursor is held by the client and is not a capability: it carries only
//! a position, bound to the read, the filter and the order key, and a cursor
//! that does not match all three is refused as `<read>_cursor_invalid`. It is
//! unauthenticated by design. Decoding one grants nothing, and the read
//! re-authorizes every page, so a forged position can only skip or repeat
//! rows the caller may already see.

use std::collections::BTreeMap;

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::handler_error::HandlerError;

/// The envelope's own leaves, the same for every read. A disclosure tier
/// treats these uniformly; a read's `rows` or `record` leaves are tiered by
/// the read itself.
pub const ENVELOPE_LEAVES: &[&str] = &[
    "/schema",
    "/as_of",
    "/revision",
    "/coverage/state",
    "/coverage/reason",
    "/page/next_cursor",
    "/page/total",
    "/page/total_basis",
    "/signers/*/label",
    "/signers/*/role",
    "/signers/*/status",
    "/not_modified",
];

/// Rows a page holds when the caller names no limit.
pub const DEFAULT_LIMIT: usize = 50;
/// Most rows one page holds.
pub const MAX_LIMIT: usize = 200;
/// Longest cursor accepted, in bytes.
pub const MAX_CURSOR_BYTES: usize = 1024;

/// How complete an answer is. A closed set, shared with the client's
/// wording table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoverageState {
    /// Everything the source holds that the filter admits is accounted for:
    /// an empty answer is verified empty.
    Complete,
    /// What was observed is shown; it may not be everything.
    Observed,
    /// Part of what exists is shown.
    Partial,
    /// The source could not be read.
    Failed,
    /// The source is not available on this node.
    Unavailable,
}

/// How complete an answer is and why. `reason` is a closed code, never free
/// text. A read adds typed facts about completeness under `extra` (for
/// example a source's truncation rows).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Coverage {
    pub state: CoverageState,
    pub reason: Option<&'static str>,
    #[serde(flatten)]
    pub extra: BTreeMap<&'static str, Value>,
}

impl Coverage {
    /// Everything the filter admits is accounted for.
    pub fn complete() -> Self {
        Self {
            state: CoverageState::Complete,
            reason: None,
            extra: BTreeMap::new(),
        }
    }

    /// What was observed, with no claim that it is everything.
    pub fn observed() -> Self {
        Self {
            state: CoverageState::Observed,
            reason: None,
            extra: BTreeMap::new(),
        }
    }

    /// A partial, failed or unavailable answer with its code. A code is lower
    /// snake case, as every stable refusal code is.
    pub fn with_reason(state: CoverageState, reason: &'static str) -> Self {
        debug_assert!(is_code(reason), "coverage reason {reason:?} is not a code");
        Self {
            state,
            reason: Some(reason),
            extra: BTreeMap::new(),
        }
    }

    /// Add a typed completeness fact. `state` and `reason` are the
    /// coverage's own fields and are never overwritten: such a key is
    /// dropped.
    pub fn with_extra(mut self, key: &'static str, value: Value) -> Self {
        debug_assert!(
            !COVERAGE_OWN_KEYS.contains(&key),
            "coverage extra may not be named {key:?}"
        );
        if !COVERAGE_OWN_KEYS.contains(&key) {
            self.extra.insert(key, value);
        }
        self
    }
}

const COVERAGE_OWN_KEYS: &[&str] = &["state", "reason"];

fn is_code(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

/// What `page.total` counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TotalBasis {
    Exact,
    AtLeast,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Page {
    pub next_cursor: Option<String>,
    pub total: Option<u64>,
    pub total_basis: TotalBasis,
}

/// A signer's standing as read when the answer was built.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignerStatus {
    Active,
    Retired,
    Revoked,
    /// The node's own key.
    Node,
    /// A fingerprint the registry does not hold.
    Unknown,
}

/// One entry of the signers side table: label, role and status only. A
/// key's owner principal is never here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SignerEntry {
    pub label: Option<String>,
    pub role: Option<String>,
    pub status: SignerStatus,
}

/// The answer of one general read.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ReadEnvelope<R> {
    pub schema: &'static str,
    pub as_of: String,
    pub revision: String,
    pub coverage: Coverage,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rows: Option<Vec<R>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub record: Option<R>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page: Option<Page>,
    pub signers: BTreeMap<String, SignerEntry>,
}

impl<R> ReadEnvelope<R> {
    /// A listing answer.
    pub fn rows(
        schema: &'static str,
        revision: String,
        coverage: Coverage,
        rows: Vec<R>,
        page: Page,
        signers: BTreeMap<String, SignerEntry>,
    ) -> Self {
        Self {
            schema,
            as_of: lillux::time::iso8601_now(),
            revision,
            coverage,
            rows: Some(rows),
            record: None,
            page: Some(page),
            signers,
        }
    }

    /// A single-record answer.
    pub fn record(
        schema: &'static str,
        revision: String,
        coverage: Coverage,
        record: R,
        signers: BTreeMap<String, SignerEntry>,
    ) -> Self {
        Self {
            schema,
            as_of: lillux::time::iso8601_now(),
            revision,
            coverage,
            rows: None,
            record: Some(record),
            page: None,
            signers,
        }
    }
}

/// The answer to a caller whose `if_revision` matches the state now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NotModified {
    pub not_modified: bool,
    pub revision: String,
}

/// `Some` when the caller's `if_revision` is the current revision. Call it
/// only after authorization, with the revision of the caller's filter.
pub fn not_modified(if_revision: Option<&str>, revision: &str) -> Option<NotModified> {
    (if_revision == Some(revision)).then(|| NotModified {
        not_modified: true,
        revision: revision.to_owned(),
    })
}

fn digest_hex(bytes: &[u8]) -> String {
    lillux::sha256_hex(bytes)
}

/// Digest of a filter: the canonical JSON (sorted keys) of the read's parsed
/// filter, with cursor, limit and `if_revision` left out by the caller, so a
/// field order or map order never changes it.
pub fn filter_digest(filter: &impl Serialize) -> Result<String, HandlerError> {
    let value = serde_json::to_value(filter)
        .map_err(|error| HandlerError::Internal(format!("filter does not serialize: {error}")))?;
    let canonical = lillux::canonical_json(&value).map_err(|error| {
        HandlerError::Internal(format!("filter is not canonical JSON: {error}"))
    })?;
    Ok(digest_hex(canonical.as_bytes()))
}

/// Digest of what a caller may see: its key, its effective scopes and the
/// node policy generation it was judged under. Folded into every revision,
/// so narrowing a caller's scope, removing its grant, retiring its key or
/// replacing the policy changes the revision even when no source moved.
pub fn authorization_digest(
    caller_key: &str,
    effective_scopes: &[String],
    policy_generation: &str,
) -> String {
    let mut scopes = effective_scopes.to_vec();
    scopes.sort();
    scopes.dedup();
    let mut bytes = Vec::new();
    for part in [caller_key, policy_generation] {
        bytes.extend_from_slice(part.as_bytes());
        bytes.push(0);
    }
    for scope in &scopes {
        bytes.extend_from_slice(scope.as_bytes());
        bytes.push(0);
    }
    digest_hex(&bytes)
}

/// The revision of the state a caller's filter reads: a digest over every
/// source tip the read draws on, named, and the filter. A change in any
/// source changes it; another filter has another revision.
///
/// `authorization` is [`authorization_digest`] of the caller, so a change of
/// the caller's grant, scope, key standing or the node policy never leaves a
/// stale `not_modified` standing; a seat also names its pins among `tips`.
///
/// Source tips are node-wide, so a polling caller can learn that something
/// changed beyond its own rows. That is acceptable for operator-tier reads;
/// a principal-tier read should pass tips scoped to the caller where that is
/// cheap.
pub fn revision_of(
    read: &str,
    authorization: &str,
    tips: &[(&str, &str)],
    filter_digest: &str,
) -> String {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(read.as_bytes());
    bytes.push(0);
    bytes.extend_from_slice(authorization.as_bytes());
    bytes.push(0);
    for (name, tip) in tips {
        bytes.extend_from_slice(name.as_bytes());
        bytes.push(b'=');
        bytes.extend_from_slice(tip.as_bytes());
        bytes.push(0);
    }
    bytes.extend_from_slice(filter_digest.as_bytes());
    format!("rev1:{}", digest_hex(&bytes))
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CursorPayload {
    read: String,
    filter_digest: String,
    order: String,
    position: String,
}

/// Encodes and checks one read's cursors.
#[derive(Debug, Clone, Copy)]
pub struct CursorCodec {
    /// The read's name, used in the refusal code: `<read>_cursor_invalid`.
    pub read: &'static str,
}

impl CursorCodec {
    pub const fn new(read: &'static str) -> Self {
        Self { read }
    }

    fn invalid(&self, why: &str) -> HandlerError {
        HandlerError::BadRequest(format!(
            "{}_cursor_invalid: {why}; start again without a cursor",
            self.read
        ))
    }

    /// An opaque cursor for `position` under this filter and order key.
    pub fn encode(&self, filter_digest: &str, order: &str, position: &str) -> String {
        let payload = CursorPayload {
            read: self.read.to_owned(),
            filter_digest: filter_digest.to_owned(),
            order: order.to_owned(),
            position: position.to_owned(),
        };
        let bytes = serde_json::to_vec(&payload).unwrap_or_default();
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    }

    /// The position a cursor names, when it was issued by this read for this
    /// filter and order key.
    pub fn decode(
        &self,
        cursor: &str,
        filter_digest: &str,
        order: &str,
    ) -> Result<String, HandlerError> {
        if cursor.is_empty() || cursor.len() > MAX_CURSOR_BYTES {
            return Err(self.invalid("the cursor is empty or too long"));
        }
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(cursor)
            .map_err(|_| self.invalid("not a cursor for this read"))?;
        let payload: CursorPayload = serde_json::from_slice(&bytes)
            .map_err(|_| self.invalid("not a cursor for this read"))?;
        if payload.read != self.read {
            return Err(self.invalid("the cursor belongs to another read"));
        }
        if payload.filter_digest != filter_digest {
            return Err(self.invalid("the cursor was issued for another filter"));
        }
        if payload.order != order {
            return Err(self.invalid("the cursor was issued for another order"));
        }
        Ok(payload.position)
    }
}

/// The limit a read uses: the caller's, within `1..=MAX_LIMIT`, else the
/// default.
pub fn page_limit(limit: Option<usize>) -> Result<usize, HandlerError> {
    match limit {
        None => Ok(DEFAULT_LIMIT),
        Some(limit) if (1..=MAX_LIMIT).contains(&limit) => Ok(limit),
        Some(_) => Err(HandlerError::BadRequest(format!(
            "limit must be 1 to {MAX_LIMIT}"
        ))),
    }
}

/// A key fingerprint in the typed form reads return: `fp:<hex>`. Accepts the
/// typed form or bare hex, both 64 lowercase hex digits; anything else is
/// refused so junk never looks like a typed key.
pub fn key_id(fingerprint: &str) -> Result<String, HandlerError> {
    let hex = fingerprint.strip_prefix("fp:").unwrap_or(fingerprint);
    if hex.len() == 64
        && hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Ok(format!("fp:{hex}"))
    } else {
        Err(HandlerError::Internal(format!(
            "{fingerprint:?} is not a key fingerprint"
        )))
    }
}

/// The signers side table for exactly `fingerprints`, resolved from the
/// node's signer registry as it stands now. A value that is not a key
/// fingerprint refuses the table.
pub fn signers_table<'a>(
    fingerprints: impl IntoIterator<Item = &'a str>,
    view: &ryeos_node::trust_registry::RegistryView,
    node_fingerprint: &str,
) -> Result<BTreeMap<String, SignerEntry>, HandlerError> {
    let node_hex = node_fingerprint
        .strip_prefix("fp:")
        .unwrap_or(node_fingerprint);
    let mut table = BTreeMap::new();
    for fingerprint in fingerprints {
        let hex = fingerprint.strip_prefix("fp:").unwrap_or(fingerprint);
        let entry = if hex == node_hex {
            SignerEntry {
                label: None,
                role: None,
                status: SignerStatus::Node,
            }
        } else if let Some(record) = view.records.iter().find(|record| record.fingerprint == hex) {
            SignerEntry {
                label: Some(record.label.clone()),
                role: Some(record.role.as_str().to_owned()),
                status: match record.status {
                    ryeos_engine::trust::SignerStatus::Active => SignerStatus::Active,
                    ryeos_engine::trust::SignerStatus::Retired => SignerStatus::Retired,
                    ryeos_engine::trust::SignerStatus::Revoked => SignerStatus::Revoked,
                },
            }
        } else {
            SignerEntry {
                label: None,
                role: None,
                status: SignerStatus::Unknown,
            }
        };
        table.insert(key_id(hex)?, entry);
    }
    Ok(table)
}

#[cfg(test)]
mod tests;
