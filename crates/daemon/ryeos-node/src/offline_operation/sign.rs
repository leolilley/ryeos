//! Building and signing a request away from the node.
//!
//! An origin operator signs against facts it read from the node (the
//! daemon's facts route, or the platform log of a node that cannot start),
//! with a key that is not the node's own. The request is checked here against
//! the same compiled table the node checks it against, so a request the node
//! is certain to refuse is not signed at all; the node still decides.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, bail, ensure};
use base64::Engine as _;
use lillux::crypto::SigningKey;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::contract::{
    DEFAULT_REQUEST_LIFETIME_SECS, MAX_REQUEST_LIFETIME_SECS, REQUEST_SCHEMA, RequestBody,
    SignedRequest,
};
use super::facts::MANDATORY_FACTS;
use super::table::lookup;

/// The grant a remote operator signs under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteSigner {
    /// Digest of the signer's grant file on the node.
    pub grant_digest: String,
    /// The origin site the grant is bound to.
    pub origin_site_id: String,
}

pub struct SignDraft {
    pub node_fingerprint: String,
    pub operation: String,
    pub arguments: Value,
    /// Input bytes by slot.
    pub inputs: BTreeMap<String, Vec<u8>>,
    pub expected_result: BTreeMap<String, String>,
    /// The node facts the request is signed against, by name.
    pub pins: BTreeMap<String, String>,
    /// How long the request stays valid, in minutes; `None` signs for
    /// [`DEFAULT_REQUEST_LIFETIME_SECS`]. The clock is the library's: a caller
    /// never reasons about time.
    pub lifetime_minutes: Option<u32>,
    /// Absent for the node's local operator.
    pub remote: Option<RemoteSigner>,
}

/// A request with the input bytes it names, as a delivery carries them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Delivery {
    pub request: SignedRequest,
    /// Input bytes by their SHA-256, base64 encoded.
    #[serde(default)]
    pub inputs: BTreeMap<String, String>,
}

impl Delivery {
    /// The input bytes by digest.
    pub fn input_bytes(&self) -> Result<BTreeMap<String, Vec<u8>>> {
        self.inputs
            .iter()
            .map(|(digest, encoded)| {
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(encoded)
                    .with_context(|| format!("input {digest} is not base64"))?;
                Ok((digest.clone(), bytes))
            })
            .collect()
    }
}

/// Sign `draft` as of the current wall clock.
pub fn sign_request_now(draft: SignDraft, key: &SigningKey) -> Result<Delivery> {
    sign_request(draft, key, lillux::time::unix_secs_now())
}

/// Sign `draft` as of `now_secs`, Unix seconds.
pub fn sign_request(draft: SignDraft, key: &SigningKey, now_secs: u64) -> Result<Delivery> {
    let lifetime = match draft.lifetime_minutes {
        None => DEFAULT_REQUEST_LIFETIME_SECS,
        Some(minutes) => u64::from(minutes) * 60,
    };
    ensure!(lifetime > 0, "the lifetime must be positive");
    ensure!(
        lifetime <= MAX_REQUEST_LIFETIME_SECS,
        "the lifetime exceeds {} hours",
        MAX_REQUEST_LIFETIME_SECS / 3600
    );
    let spec = lookup(&draft.operation)
        .with_context(|| format!("this build has no offline operation `{}`", draft.operation))?;
    let wanted: BTreeSet<&str> = MANDATORY_FACTS
        .iter()
        .chain(spec.preconditions)
        .copied()
        .collect();
    let pinned: BTreeSet<&str> = draft.pins.keys().map(String::as_str).collect();
    if wanted != pinned {
        bail!(
            "`{}` is signed against the facts {wanted:?}; {pinned:?} were given",
            spec.id
        );
    }
    let mut digests = BTreeMap::new();
    let mut delivered = BTreeMap::new();
    for (slot, bytes) in &draft.inputs {
        let definition = spec
            .slot(slot)
            .with_context(|| format!("`{}` has no input slot `{slot}`", spec.id))?;
        ensure!(
            u64::try_from(bytes.len()).unwrap_or(u64::MAX) <= definition.max_bytes,
            "input `{slot}` exceeds its {} byte bound",
            definition.max_bytes
        );
        let digest = lillux::sha256_hex(bytes);
        digests.insert(slot.clone(), digest.clone());
        delivered.insert(
            digest,
            base64::engine::general_purpose::STANDARD.encode(bytes),
        );
    }
    let (signer_grant_digest, signer_origin_site_id) = match draft.remote {
        Some(remote) => (Some(remote.grant_digest), Some(remote.origin_site_id)),
        None => (None, None),
    };
    let body = RequestBody {
        schema: REQUEST_SCHEMA.to_owned(),
        node_fingerprint: draft.node_fingerprint,
        signer_fingerprint: lillux::signature::compute_fingerprint(&key.verifying_key()),
        signer_grant_digest,
        signer_origin_site_id,
        operation: draft.operation,
        arguments: draft.arguments,
        inputs: digests,
        expected_result: draft.expected_result,
        precondition: draft.pins,
        issued_at: lillux::time::iso8601_from_unix_secs(now_secs),
        not_after: lillux::time::iso8601_from_unix_secs(now_secs + lifetime),
    };
    body.validate_shape()?;
    (spec.validate_request)(&body)?;
    let request = SignedRequest::sign(body, key)?;
    // The document must fit what a node reads.
    request.to_bytes()?;
    Ok(Delivery {
        request,
        inputs: delivered,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::offline_operation::facts::{FACT_INIT_COMPLETION, FACT_RESTART_INHIBITION};

    const NOW: u64 = 1_761_700_000;

    fn draft() -> SignDraft {
        SignDraft {
            node_fingerprint: "a".repeat(64),
            operation: "node.allow-restart".to_owned(),
            arguments: json!({}),
            inputs: BTreeMap::new(),
            expected_result: BTreeMap::new(),
            pins: BTreeMap::from([
                (FACT_INIT_COMPLETION.to_owned(), "absent".to_owned()),
                ("ledger_head".to_owned(), "b".repeat(64)),
                (FACT_RESTART_INHIBITION.to_owned(), "absent".to_owned()),
            ]),
            lifetime_minutes: None,
            remote: None,
        }
    }

    #[test]
    fn the_default_lifetime_is_one_hour() {
        let key = lillux::crypto::generate_signing_key();
        let delivery = sign_request(draft(), &key, NOW).unwrap();
        let body = &delivery.request.body;
        assert_eq!(body.issued_at().unwrap().unix_secs(), NOW);
        assert_eq!(
            body.not_after().unwrap().unix_secs() - body.issued_at().unwrap().unix_secs(),
            3600
        );
        delivery
            .request
            .verify_signature(&key.verifying_key())
            .unwrap();
    }

    #[test]
    fn a_lifetime_may_reach_a_day_and_no_further() {
        let key = lillux::crypto::generate_signing_key();
        let mut day = draft();
        day.lifetime_minutes = Some(24 * 60);
        sign_request(day, &key, NOW).unwrap();
        let mut long = draft();
        long.lifetime_minutes = Some(25 * 60);
        assert!(sign_request(long, &key, NOW).is_err());
        let mut none = draft();
        none.lifetime_minutes = Some(0);
        assert!(sign_request(none, &key, NOW).is_err());
    }

    #[test]
    fn the_pins_must_be_exactly_the_facts_the_operation_names() {
        let key = lillux::crypto::generate_signing_key();
        let mut missing = draft();
        missing.pins.remove("ledger_head");
        assert!(sign_request(missing, &key, NOW).is_err());
        let mut extra = draft();
        extra
            .pins
            .insert("policy_generation".to_owned(), "c".repeat(64));
        assert!(sign_request(extra, &key, NOW).is_err());
    }

    #[test]
    fn an_unknown_operation_or_slot_is_not_signed() {
        let key = lillux::crypto::generate_signing_key();
        let mut unknown = draft();
        unknown.operation = "node.does-not-exist".to_owned();
        assert!(sign_request(unknown, &key, NOW).is_err());
        let mut slot = draft();
        slot.inputs.insert("nothing".to_owned(), b"x".to_vec());
        assert!(sign_request(slot, &key, NOW).is_err());
    }

    #[test]
    fn a_remote_signer_names_its_grant_and_site() {
        let key = lillux::crypto::generate_signing_key();
        let mut remote = draft();
        remote.remote = Some(RemoteSigner {
            grant_digest: "d".repeat(64),
            origin_site_id: "site:origin-a".to_owned(),
        });
        let delivery = sign_request(remote, &key, NOW).unwrap();
        assert_eq!(
            delivery.request.body.signer_grant_digest.as_deref(),
            Some("d".repeat(64).as_str())
        );
    }

    #[test]
    fn a_delivery_round_trips_through_json() {
        let key = lillux::crypto::generate_signing_key();
        let delivery = sign_request(draft(), &key, NOW).unwrap();
        let text = serde_json::to_string(&delivery).unwrap();
        let again: Delivery = serde_json::from_str(&text).unwrap();
        assert_eq!(again, delivery);
        assert!(again.input_bytes().unwrap().is_empty());
    }
}
