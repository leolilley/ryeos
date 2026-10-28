//! Delivery to a running node: the checks a daemon makes before it queues a
//! signed request for the next startup.
//!
//! A submitted request is verified exactly as the startup applier verifies
//! it: node, signature, signer's grant, exact scope, delegation, time window,
//! shape, replay, the ledger head, every pinned fact and every input. It is
//! then queued in the same inbox the applier reads, under the same names, so
//! the applier checks it again against whatever the node is by then. Nothing
//! is applied here: the handlers need a stopped node.
//!
//! A running node is stricter than a starting one in one respect: a request
//! pinned to any ledger head but the current one is refused, not held, since
//! nothing it could wait for will happen before it is applied.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use ryeos_node_runtime::identity::NodeIdentity;
use serde_json::{Value, json};

use super::apply::{Ledger, check_pins};
use super::contract::{MAX_INBOX_REQUESTS, Refusal, RefusalReason, SignedRequest};
use super::envelope::Envelope;
use super::facts::{FACT_LEDGER_HEAD, measure};
use super::store::{Read, Store};
use super::table::lookup;

/// How many receipts the status report lists.
const STATUS_RECEIPTS: usize = 16;

/// The daemon's means of stopping itself so the next startup applies what is
/// queued. It is a plain operator stop: an orderly exit with no restart
/// inhibition, which the node's supervisor answers by starting the next
/// incarnation. The composition root supplies it; this crate does not know
/// how the process is shut down.
pub struct StopForOfflineOperation(Box<dyn Fn() + Send + Sync>);

impl StopForOfflineOperation {
    pub fn new(stop: impl Fn() + Send + Sync + 'static) -> Self {
        Self(Box::new(stop))
    }

    pub fn request(&self) {
        (self.0)();
    }
}

/// Why a submission was not queued.
#[derive(Debug)]
pub enum DeliveryError {
    /// The request failed a check; nothing was written.
    Refused(Refusal),
    /// The node could not read or write its own state.
    Failed(anyhow::Error),
}

impl From<Refusal> for DeliveryError {
    fn from(refusal: Refusal) -> Self {
        Self::Refused(refusal)
    }
}

/// What was queued.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Accepted {
    /// The request's identity: the digest of its canonical body.
    pub request_digest: String,
    pub operation: String,
    /// The ledger head the node was at, which the request is pinned to.
    pub ledger_head: String,
    /// The same request was already in the inbox; nothing was written.
    pub already_queued: bool,
}

/// Verify a submitted request and queue it with its inputs.
///
/// `submitted_by` is the fingerprint of the key that authenticated the
/// delivery. It must be the request's signer: a delivery cannot launder
/// another key's request through its own authority.
pub fn accept(
    app_root: &Path,
    node: &NodeIdentity,
    submitted_by: &str,
    request: &SignedRequest,
    inputs: &BTreeMap<String, Vec<u8>>,
    now: DateTime<Utc>,
) -> Result<Accepted, DeliveryError> {
    let bytes = request.to_bytes().map_err(|error| {
        DeliveryError::Refused(Refusal::new(RefusalReason::Malformed, format!("{error:#}")))
    })?;
    // The bytes are what the applier reads back, so they are what is checked.
    let request = SignedRequest::parse(&bytes).map_err(|error| {
        DeliveryError::Refused(Refusal::new(RefusalReason::Malformed, format!("{error:#}")))
    })?;
    let body = &request.body;
    if body.signer_fingerprint != submitted_by {
        return Err(Refusal::new(
            RefusalReason::UnknownSigner,
            "the request is signed by another key than the one delivering it",
        )
        .into());
    }
    Envelope {
        app_root,
        node,
        now,
        enforce_clock: true,
    }
    .check(&request)?;
    let request_digest = request
        .digest()
        .map_err(|error| DeliveryError::Failed(error.context("digest the request")))?;
    let spec = lookup(&body.operation).ok_or_else(|| {
        Refusal::new(
            RefusalReason::UnknownOperation,
            format!("this node has no operation `{}`", body.operation),
        )
    })?;

    let store = Store::for_app_root(app_root);
    store
        .create_layout()
        .map_err(|error| DeliveryError::Failed(error.context("prepare the inbox")))?;
    let state = Ledger::new(&store, node)
        .load()
        .map_err(|error| DeliveryError::Failed(error.context("verify the ledger")))?;
    if state.request_digests.contains(&request_digest) {
        return Err(Refusal::new(
            RefusalReason::Replayed,
            "the request digest is already in the ledger",
        )
        .into());
    }
    let pinned = body.precondition.get(FACT_LEDGER_HEAD);
    if pinned != Some(&state.head) {
        return Err(Refusal::new(
            RefusalReason::StaleLedgerHead,
            "the request was not signed against this node's current ledger head",
        )
        .into());
    }
    check_pins(app_root, spec, body, &state.head)?;

    let wanted: BTreeSet<&String> = body.inputs.values().collect();
    for given in inputs.keys() {
        if !wanted.contains(given) {
            return Err(Refusal::new(
                RefusalReason::InvalidInputs,
                format!("input {given} is not one the request names"),
            )
            .into());
        }
    }
    for (slot, digest) in &body.inputs {
        let definition = spec.slot(slot).ok_or_else(|| {
            Refusal::new(
                RefusalReason::InvalidInputs,
                format!("`{}` has no input slot `{slot}`", spec.id),
            )
        })?;
        let Some(bytes) = inputs.get(digest) else {
            return Err(Refusal::new(
                RefusalReason::InputMissing,
                format!("input `{slot}` ({digest}) was not delivered"),
            )
            .into());
        };
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > definition.max_bytes {
            return Err(Refusal::new(
                RefusalReason::InputOversized,
                format!(
                    "input `{slot}` exceeds its {} byte bound",
                    definition.max_bytes
                ),
            )
            .into());
        }
        if lillux::sha256_hex(bytes) != *digest {
            return Err(Refusal::new(
                RefusalReason::InputMismatch,
                format!("input `{slot}` does not hash to the digest the request pinned"),
            )
            .into());
        }
    }

    let name = lillux::sha256_hex(&bytes);
    let accepted = Accepted {
        request_digest,
        operation: body.operation.clone(),
        ledger_head: state.head.clone(),
        already_queued: false,
    };
    match store.read_request(&name) {
        Ok(Read::Missing) => {}
        Ok(Read::Bytes(queued)) if queued == bytes => {
            return Ok(Accepted {
                already_queued: true,
                ..accepted
            });
        }
        Ok(Read::Bytes(_) | Read::TooLarge) => {
            return Err(DeliveryError::Failed(anyhow::anyhow!(
                "the inbox holds another document under the name {name}"
            )));
        }
        Err(error) => return Err(DeliveryError::Failed(error.context("read the inbox"))),
    }
    let queued = store
        .list_inbox()
        .map_err(|error| DeliveryError::Failed(error.context("list the inbox")))?;
    if queued.overflow || queued.requests.len() >= MAX_INBOX_REQUESTS {
        return Err(Refusal::new(
            RefusalReason::InboxFull,
            format!("the inbox already holds {MAX_INBOX_REQUESTS} requests"),
        )
        .into());
    }
    for (digest, bytes) in inputs {
        store
            .queue_input(digest, bytes)
            .map_err(|error| DeliveryError::Failed(error.context("queue an input")))?;
    }
    store
        .queue_request(&name, &bytes)
        .map_err(|error| DeliveryError::Failed(error.context("queue the request")))?;
    Ok(accepted)
}

fn local_node(app_root: &Path) -> Result<NodeIdentity> {
    super::apply::node_identity(app_root)
        .map_err(|error| anyhow::anyhow!("{error}"))?
        .context("the node has no identity; it is not initialized")
}

/// [`facts_report`] for a node read from its volume, running or not.
pub fn local_facts_report(app_root: &Path) -> Result<Value> {
    facts_report(app_root, &local_node(app_root)?)
}

/// [`status_report`] for a node read from its volume, running or not.
pub fn local_status_report(app_root: &Path) -> Result<Value> {
    status_report(app_root, &local_node(app_root)?)
}

/// The node's offline-operation facts and ledger tip, for the operator who
/// signs against them.
pub fn facts_report(app_root: &Path, node: &NodeIdentity) -> Result<Value> {
    let store = Store::for_app_root(app_root);
    let state = Ledger::new(&store, node)
        .load()
        .context("verify the offline-operation ledger")?;
    Ok(json!({
        "node_fingerprint": node.fingerprint(),
        "facts": measure(app_root, &state.head),
        "ledger_head": state.head,
        "ledger_sequence": state.last_sequence,
    }))
}

/// The ledger tip, what waits in the inbox, the newest receipts and the
/// refusals on record. Never input bytes.
pub fn status_report(app_root: &Path, node: &NodeIdentity) -> Result<Value> {
    let store = Store::for_app_root(app_root);
    let ledger = Ledger::new(&store, node);
    let state = ledger
        .load()
        .context("verify the offline-operation ledger")?;
    let mut queued = Vec::new();
    for name in store.list_inbox()?.requests {
        let Ok(Read::Bytes(bytes)) = store.read_request(&name) else {
            continue;
        };
        let Ok(request) = SignedRequest::parse(&bytes) else {
            continue;
        };
        queued.push(json!({
            "request_digest": request.digest()?,
            "operation": request.body.operation,
            "signer_fingerprint": request.body.signer_fingerprint,
            "not_after": request.body.not_after,
        }));
    }
    let receipts: Vec<Value> = ledger
        .recent(&state, STATUS_RECEIPTS)?
        .iter()
        .map(|receipt| {
            json!({
                "sequence": receipt.body.sequence,
                "request_digest": receipt.body.request_digest,
                "operation": receipt.body.request.body.operation,
                "recorded_at": receipt.body.recorded_at,
            })
        })
        .collect();
    let refusals: Vec<Value> = store
        .refusals()?
        .iter()
        .map(|refusal| {
            json!({
                "request_digest": refusal.body.request_digest,
                "reason": refusal.body.reason.code(),
                "occurrences": refusal.body.occurrences,
                "last_seen": refusal.body.last_seen,
            })
        })
        .collect();
    Ok(json!({
        "node_fingerprint": node.fingerprint(),
        "ledger_head": state.head,
        "ledger_sequence": state.last_sequence,
        "queued": queued,
        "receipts": receipts,
        "refusals": refusals,
    }))
}

#[cfg(test)]
mod tests {
    use base64::Engine as _;
    use chrono::Duration;
    use lillux::crypto::SigningKey;
    use ryeos_engine::roots::RuntimeRoot;
    use ryeos_node_runtime::identity::{WildcardPolicy, reconcile_authorized_key_toml_scopes};

    use super::*;
    use crate::offline_operation::contract::{REQUEST_SCHEMA, RequestBody};
    use crate::offline_operation::envelope::authorized_keys_dir;
    use crate::offline_operation::facts::MANDATORY_FACTS;
    use crate::offline_operation::read_facts;

    const SITE: &str = "site:origin-a";
    const RESTART_SCOPE: &str = "ryeos.offline.node.allow-restart";

    struct Fixture {
        root: tempfile::TempDir,
        node: NodeIdentity,
        operator: SigningKey,
    }

    fn fixture() -> Fixture {
        let root = tempfile::tempdir().unwrap();
        let runtime = RuntimeRoot::new(root.path().to_path_buf());
        let node = NodeIdentity::create(&runtime.node_signing_key_path()).unwrap();
        let operator =
            lillux::crypto::create_signing_key(&runtime.operator_signing_key_path()).unwrap();
        std::fs::create_dir_all(runtime.state()).unwrap();
        Fixture {
            root,
            node,
            operator,
        }
    }

    fn fingerprint(key: &SigningKey) -> String {
        lillux::signature::compute_fingerprint(&key.verifying_key())
    }

    fn stamp(offset: Duration) -> String {
        (Utc::now() + offset).to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    }

    impl Fixture {
        fn path(&self) -> &Path {
            self.root.path()
        }

        fn body(&self) -> RequestBody {
            let facts = read_facts(self.path()).unwrap();
            let spec = lookup("node.allow-restart").unwrap();
            RequestBody {
                schema: REQUEST_SCHEMA.to_owned(),
                node_fingerprint: self.node.fingerprint().to_owned(),
                signer_fingerprint: fingerprint(&self.operator),
                signer_grant_digest: None,
                signer_origin_site_id: None,
                operation: "node.allow-restart".to_owned(),
                arguments: json!({}),
                inputs: BTreeMap::new(),
                expected_result: BTreeMap::new(),
                precondition: MANDATORY_FACTS
                    .iter()
                    .chain(spec.preconditions)
                    .map(|fact| ((*fact).to_owned(), facts[*fact].clone()))
                    .collect(),
                issued_at: stamp(Duration::seconds(-30)),
                not_after: stamp(Duration::hours(1)),
            }
        }

        fn signed(&self, body: RequestBody) -> SignedRequest {
            SignedRequest::sign(body, &self.operator).unwrap()
        }

        fn submit(&self, request: &SignedRequest) -> Result<Accepted, DeliveryError> {
            accept(
                self.path(),
                &self.node,
                &request.body.signer_fingerprint,
                request,
                &BTreeMap::new(),
                Utc::now(),
            )
        }

        fn grant(&self, key: &SigningKey, scopes: &[&str], wildcard: WildcardPolicy) -> String {
            let fp = fingerprint(key);
            let encoded =
                base64::engine::general_purpose::STANDARD.encode(key.verifying_key().as_bytes());
            let scopes: Vec<String> = scopes.iter().map(|s| (*s).to_owned()).collect();
            let directory = authorized_keys_dir(self.path());
            reconcile_authorized_key_toml_scopes(
                &directory,
                &fp,
                &encoded,
                &scopes,
                "remote operator",
                "test",
                "2026-01-01T00:00:00Z",
                &self.node,
                wildcard,
                false,
                Some(SITE),
                true,
            )
            .unwrap();
            lillux::sha256_hex(&std::fs::read(directory.join(format!("{fp}.toml"))).unwrap())
        }

        fn inbox(&self) -> Vec<String> {
            let mut names: Vec<String> =
                std::fs::read_dir(Store::for_app_root(self.path()).inbox_dir())
                    .map(|entries| {
                        entries
                            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                            .collect()
                    })
                    .unwrap_or_default();
            names.sort();
            names
        }
    }

    fn refused(result: Result<Accepted, DeliveryError>) -> RefusalReason {
        match result {
            Err(DeliveryError::Refused(refusal)) => refusal.reason,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_valid_request_is_queued_for_the_next_startup() {
        let fx = fixture();
        let request = fx.signed(fx.body());
        let accepted = fx.submit(&request).unwrap();
        assert_eq!(accepted.request_digest, request.digest().unwrap());
        assert_eq!(accepted.operation, "node.allow-restart");
        assert!(!accepted.already_queued);
        let name = format!("{}.json", lillux::sha256_hex(&request.to_bytes().unwrap()));
        assert_eq!(fx.inbox(), [name]);
        // The startup applier takes it from there.
        let report = crate::offline_operation::apply_pending_offline_operations(
            fx.path(),
            &crate::offline_operation::BareHost,
        )
        .unwrap();
        assert_eq!(report.applied.len(), 1, "{:?}", report.refused);
        assert!(fx.inbox().is_empty());
    }

    #[test]
    fn submitting_twice_queues_once() {
        let fx = fixture();
        let request = fx.signed(fx.body());
        fx.submit(&request).unwrap();
        let again = fx.submit(&request).unwrap();
        assert!(again.already_queued);
        assert_eq!(fx.inbox().len(), 1);
    }

    #[test]
    fn a_forged_signature_is_refused_and_nothing_is_queued() {
        let fx = fixture();
        let mut request = fx.signed(fx.body());
        request.body.arguments = json!({ "tampered": true });
        assert_eq!(refused(fx.submit(&request)), RefusalReason::BadSignature);
        assert!(fx.inbox().is_empty());
    }

    #[test]
    fn a_request_for_another_node_is_refused() {
        let fx = fixture();
        let mut body = fx.body();
        body.node_fingerprint = "c".repeat(64);
        assert_eq!(
            refused(fx.submit(&fx.signed(body))),
            RefusalReason::WrongNode
        );
        assert!(fx.inbox().is_empty());
    }

    #[test]
    fn a_request_signed_against_another_ledger_head_is_refused() {
        let fx = fixture();
        let mut body = fx.body();
        body.precondition
            .insert("ledger_head".to_owned(), "d".repeat(64));
        assert_eq!(
            refused(fx.submit(&fx.signed(body))),
            RefusalReason::StaleLedgerHead
        );
        assert!(fx.inbox().is_empty());
    }

    #[test]
    fn a_request_behind_an_applied_one_is_stale_and_an_applied_one_is_a_replay() {
        let fx = fixture();
        let stale = fx.signed(fx.body());
        let first = fx.signed({
            let mut body = fx.body();
            body.issued_at = stamp(Duration::seconds(-20));
            body
        });
        fx.submit(&first).unwrap();
        crate::offline_operation::apply_pending_offline_operations(
            fx.path(),
            &crate::offline_operation::BareHost,
        )
        .unwrap();
        assert_eq!(refused(fx.submit(&stale)), RefusalReason::StaleLedgerHead);
        // Resubmitting the applied request is a replay.
        assert_eq!(refused(fx.submit(&first)), RefusalReason::Replayed);
    }

    #[test]
    fn an_expired_request_is_refused() {
        let fx = fixture();
        let mut body = fx.body();
        body.issued_at = stamp(Duration::hours(-3));
        body.not_after = stamp(Duration::hours(-2));
        assert_eq!(
            refused(fx.submit(&fx.signed(body))),
            RefusalReason::Expired
        );
    }

    #[test]
    fn a_changed_fact_is_refused() {
        let fx = fixture();
        let mut body = fx.body();
        body.precondition
            .insert("init_completion".to_owned(), "e".repeat(64));
        assert_eq!(
            refused(fx.submit(&fx.signed(body))),
            RefusalReason::PreconditionFailed
        );
    }

    #[test]
    fn wildcard_grants_never_satisfy_an_offline_scope() {
        let fx = fixture();
        for scopes in [vec!["*"], vec!["ryeos.*"]] {
            let key = lillux::crypto::generate_signing_key();
            fx.grant(&key, &scopes, WildcardPolicy::AllowBootstrap);
            let mut body = fx.body();
            body.signer_fingerprint = fingerprint(&key);
            body.signer_grant_digest = Some(
                lillux::sha256_hex(
                    &std::fs::read(
                        authorized_keys_dir(fx.path())
                            .join(format!("{}.toml", fingerprint(&key))),
                    )
                    .unwrap(),
                ),
            );
            body.signer_origin_site_id = Some(SITE.to_owned());
            let request = SignedRequest::sign(body, &key).unwrap();
            assert_eq!(
                refused(fx.submit(&request)),
                RefusalReason::ScopeNotGranted,
                "{scopes:?}"
            );
        }
        assert!(fx.inbox().is_empty());
    }

    #[test]
    fn a_remote_operator_with_the_exact_scope_is_queued() {
        let fx = fixture();
        let key = lillux::crypto::generate_signing_key();
        let grant = fx.grant(&key, &[RESTART_SCOPE], WildcardPolicy::Reject);
        let mut body = fx.body();
        body.signer_fingerprint = fingerprint(&key);
        body.signer_grant_digest = Some(grant);
        body.signer_origin_site_id = Some(SITE.to_owned());
        let request = SignedRequest::sign(body, &key).unwrap();
        fx.submit(&request).unwrap();
        assert_eq!(fx.inbox().len(), 1);
    }

    #[test]
    fn a_delivery_cannot_carry_another_keys_request() {
        let fx = fixture();
        let request = fx.signed(fx.body());
        let result = accept(
            fx.path(),
            &fx.node,
            &"f".repeat(64),
            &request,
            &BTreeMap::new(),
            Utc::now(),
        );
        assert_eq!(refused(result), RefusalReason::UnknownSigner);
    }

    #[test]
    fn an_input_the_request_does_not_name_is_refused() {
        let fx = fixture();
        let request = fx.signed(fx.body());
        let stray = b"stray".to_vec();
        let result = accept(
            fx.path(),
            &fx.node,
            &request.body.signer_fingerprint,
            &request,
            &BTreeMap::from([(lillux::sha256_hex(&stray), stray)]),
            Utc::now(),
        );
        assert_eq!(refused(result), RefusalReason::InvalidInputs);
        assert!(fx.inbox().is_empty());
    }

    #[test]
    fn a_full_inbox_refuses_further_requests() {
        let fx = fixture();
        let store = Store::for_app_root(fx.path());
        store.create_layout().unwrap();
        for index in 0..MAX_INBOX_REQUESTS {
            std::fs::write(
                store.inbox_dir().join(format!("{index:064x}.json")),
                b"{}",
            )
            .unwrap();
        }
        let request = fx.signed(fx.body());
        assert_eq!(refused(fx.submit(&request)), RefusalReason::InboxFull);
    }

    #[test]
    fn the_status_report_lists_what_waits_and_what_was_applied() {
        let fx = fixture();
        let request = fx.signed(fx.body());
        fx.submit(&request).unwrap();
        let waiting = status_report(fx.path(), &fx.node).unwrap();
        assert_eq!(waiting["queued"].as_array().unwrap().len(), 1);
        crate::offline_operation::apply_pending_offline_operations(
            fx.path(),
            &crate::offline_operation::BareHost,
        )
        .unwrap();
        let done = status_report(fx.path(), &fx.node).unwrap();
        assert!(done["queued"].as_array().unwrap().is_empty());
        assert_eq!(done["receipts"].as_array().unwrap().len(), 1);
        assert_eq!(done["ledger_sequence"], 1);
        let facts = facts_report(fx.path(), &fx.node).unwrap();
        assert_eq!(facts["ledger_head"], done["ledger_head"]);
    }
}
