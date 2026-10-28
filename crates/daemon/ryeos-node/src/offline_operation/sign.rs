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
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use lillux::crypto::SigningKey;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::contract::{
    DEFAULT_REQUEST_LIFETIME, MAX_REQUEST_LIFETIME, REQUEST_SCHEMA, RequestBody, SignedRequest,
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
    /// `None` signs for [`DEFAULT_REQUEST_LIFETIME`].
    pub lifetime: Option<Duration>,
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

pub fn sign_request(draft: SignDraft, key: &SigningKey, now: DateTime<Utc>) -> Result<Delivery> {
    let lifetime = draft.lifetime.unwrap_or(DEFAULT_REQUEST_LIFETIME);
    ensure!(lifetime > Duration::zero(), "the lifetime must be positive");
    ensure!(
        lifetime <= MAX_REQUEST_LIFETIME,
        "the lifetime exceeds {} hours",
        MAX_REQUEST_LIFETIME.num_hours()
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
        issued_at: now.to_rfc3339_opts(SecondsFormat::Secs, true),
        not_after: (now + lifetime).to_rfc3339_opts(SecondsFormat::Secs, true),
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
            lifetime: None,
            remote: None,
        }
    }

    #[test]
    fn the_default_lifetime_is_one_hour() {
        let key = lillux::crypto::generate_signing_key();
        let delivery = sign_request(draft(), &key, Utc::now()).unwrap();
        let body = &delivery.request.body;
        assert_eq!(
            body.not_after().unwrap() - body.issued_at().unwrap(),
            Duration::hours(1)
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
        day.lifetime = Some(Duration::hours(24));
        sign_request(day, &key, Utc::now()).unwrap();
        let mut long = draft();
        long.lifetime = Some(Duration::hours(25));
        assert!(sign_request(long, &key, Utc::now()).is_err());
        let mut none = draft();
        none.lifetime = Some(Duration::zero());
        assert!(sign_request(none, &key, Utc::now()).is_err());
    }

    #[test]
    fn the_pins_must_be_exactly_the_facts_the_operation_names() {
        let key = lillux::crypto::generate_signing_key();
        let mut missing = draft();
        missing.pins.remove("ledger_head");
        assert!(sign_request(missing, &key, Utc::now()).is_err());
        let mut extra = draft();
        extra.pins.insert("policy_generation".to_owned(), "c".repeat(64));
        assert!(sign_request(extra, &key, Utc::now()).is_err());
    }

    #[test]
    fn an_unknown_operation_or_slot_is_not_signed() {
        let key = lillux::crypto::generate_signing_key();
        let mut unknown = draft();
        unknown.operation = "node.does-not-exist".to_owned();
        assert!(sign_request(unknown, &key, Utc::now()).is_err());
        let mut slot = draft();
        slot.inputs.insert("nothing".to_owned(), b"x".to_vec());
        assert!(sign_request(slot, &key, Utc::now()).is_err());
    }

    #[test]
    fn a_remote_signer_names_its_grant_and_site() {
        let key = lillux::crypto::generate_signing_key();
        let mut remote = draft();
        remote.remote = Some(RemoteSigner {
            grant_digest: "d".repeat(64),
            origin_site_id: "site:origin-a".to_owned(),
        });
        let delivery = sign_request(remote, &key, Utc::now()).unwrap();
        assert_eq!(
            delivery.request.body.signer_grant_digest.as_deref(),
            Some("d".repeat(64).as_str())
        );
    }

    #[test]
    fn a_delivery_round_trips_through_json() {
        let key = lillux::crypto::generate_signing_key();
        let delivery = sign_request(draft(), &key, Utc::now()).unwrap();
        let text = serde_json::to_string(&delivery).unwrap();
        let again: Delivery = serde_json::from_str(&text).unwrap();
        assert_eq!(again, delivery);
        assert!(again.input_bytes().unwrap().is_empty());
    }
}
