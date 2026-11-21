//! The hash-chained, append-only ledger of applied operations.
//!
//! Each receipt carries the digest of the one before it, and is signed with
//! the node key when it is written. The ledger lives in a directory only the
//! application writes: this module is a child of the application so that its
//! writing half is invisible to the handlers and to every reset. Reading is
//! open to anything that wants the chain tip.

use std::collections::BTreeSet;

use anyhow::Result;
use lillux::crypto::VerifyingKey;
use ryeos_node_runtime::identity::NodeIdentity;
use serde_json::Value;

use crate::offline_operation::contract::{
    CHECKPOINT_SCHEMA, CanonicalInstant, MAX_RECEIPT_BYTES, RECEIPT_SCHEMA, ReceiptBody,
    SignedReceipt, SignedRequest, genesis_ledger_head,
};
use crate::offline_operation::store::Store;
use crate::signed_chain::{Chain, ChainEntry};

/// Receipts kept before rotation drops the oldest into a checkpoint.
pub const LEDGER_ROTATE_AT: usize = 1024;
/// Receipts kept after a rotation.
pub const LEDGER_KEEP: usize = 512;

impl ChainEntry for SignedReceipt {
    const NOUN: &'static str = "receipt";
    const LEDGER_NAME: &'static str = "offline-operation ledger";
    const SCHEMA: &'static str = RECEIPT_SCHEMA;
    const CHECKPOINT_SCHEMA: &'static str = CHECKPOINT_SCHEMA;
    const MAX_BYTES: u64 = MAX_RECEIPT_BYTES;
    const ROTATE_AT: usize = LEDGER_ROTATE_AT;
    const KEEP: usize = LEDGER_KEEP;

    fn genesis_head(node_fingerprint: &str) -> String {
        genesis_ledger_head(node_fingerprint)
    }

    fn sequence(&self) -> u64 {
        self.body.sequence
    }

    fn previous(&self) -> &str {
        &self.body.previous
    }

    fn node_fingerprint(&self) -> &str {
        &self.body.node_fingerprint
    }

    fn schema(&self) -> &str {
        &self.body.schema
    }

    fn recorded_at(&self) -> &str {
        &self.body.recorded_at
    }

    fn digest(&self) -> Result<String> {
        SignedReceipt::digest(self)
    }

    fn verify(&self, key: &VerifyingKey) -> Result<()> {
        SignedReceipt::verify(self, key)
    }
}

/// What a verified read of the ledger found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerState {
    /// Digest of the last receipt (or the checkpoint's tip, or the genesis).
    pub head: String,
    pub last_sequence: u64,
    /// Request digests of the retained receipts.
    pub request_digests: BTreeSet<String>,
    /// Digests of the retained receipts and the checkpoint tip: the heads a
    /// request may have been signed against that the ledger has since passed.
    pub past_heads: BTreeSet<String>,
    pub retained: usize,
    pub dropped_receipts: u64,
    /// When the oldest retained receipt was recorded.
    pub oldest_recorded_at: Option<String>,
    /// When the checkpoint that stands for the dropped receipts was written;
    /// present exactly when rotation has dropped any.
    pub checkpoint_recorded_at: Option<String>,
}

impl LedgerState {
    /// Whether a request issued at `issued_at` can only have been signed
    /// against a head that rotation has since dropped. A head the ledger has
    /// not reached cannot be older than the receipts it retains, so a request
    /// issued before them and pinned to an unknown head names a dropped one.
    pub fn predates_retained(&self, issued_at: CanonicalInstant) -> bool {
        let Some(checkpoint) = &self.checkpoint_recorded_at else {
            return false;
        };
        let reference = self.oldest_recorded_at.as_ref().unwrap_or(checkpoint);
        CanonicalInstant::parse("recorded_at", reference)
            .is_ok_and(|reference| issued_at <= reference)
    }

    /// Whether `head` was once this ledger's head and no longer is.
    pub fn is_past_head(&self, head: &str) -> bool {
        head != self.head && self.past_heads.contains(head)
    }
}

pub struct Ledger<'a> {
    chain: Chain<'a, SignedReceipt>,
    node: &'a NodeIdentity,
}

#[cfg(test)]
const CHECKPOINT_FILE: &str = "checkpoint.json";

#[cfg(test)]
fn receipt_name(sequence: u64) -> String {
    format!("{sequence:020}.json")
}

impl<'a> Ledger<'a> {
    pub fn new(store: &Store, node: &'a NodeIdentity) -> Self {
        Self {
            chain: Chain::new(store.ledger_dir(), node),
            node,
        }
    }

    /// Read and verify the whole retained chain: signatures, sequence,
    /// links, and the node each receipt names.
    pub fn load(&self) -> Result<LedgerState> {
        let mut request_digests = BTreeSet::new();
        let mut heads = Vec::new();
        let tip = self.chain.walk(|receipt, digest| {
            request_digests.insert(receipt.body.request_digest.clone());
            heads.push(digest.to_owned());
        })?;
        // The starting head (genesis, or a checkpoint's tip) is a head the
        // ledger has been at: a request pinned to it is stale once a receipt
        // follows. The current head itself is never "past" (`is_past_head`).
        let mut past_heads = BTreeSet::from([tip.start_head]);
        past_heads.extend(heads);
        Ok(LedgerState {
            head: tip.head,
            last_sequence: tip.last_sequence,
            request_digests,
            past_heads,
            retained: tip.retained,
            dropped_receipts: tip.dropped,
            oldest_recorded_at: tip.oldest_recorded_at,
            checkpoint_recorded_at: tip.checkpoint_recorded_at,
        })
    }

    /// The newest `limit` retained receipts, oldest first.
    pub fn recent(&self, state: &LedgerState, limit: usize) -> Result<Vec<SignedReceipt>> {
        self.chain.recent(state.last_sequence, limit)
    }

    /// The retained receipt for `request_digest`, if there is one.
    pub fn find_receipt(
        &self,
        state: &LedgerState,
        request_digest: &str,
    ) -> Result<Option<SignedReceipt>> {
        if !state.request_digests.contains(request_digest) {
            return Ok(None);
        }
        let mut sequences = self.chain.sequences()?;
        sequences.reverse();
        for sequence in sequences {
            if sequence > state.last_sequence {
                continue;
            }
            let receipt = self.chain.read_entry(sequence)?;
            if receipt.body.request_digest == request_digest {
                return Ok(Some(receipt));
            }
        }
        Ok(None)
    }

    /// Append one receipt after the state read under the held state lock,
    /// fsync it, and rotate when the ledger has grown past its bound. Returns
    /// the receipt and the state that now holds.
    pub(super) fn append(
        &self,
        state: &LedgerState,
        draft: ReceiptDraft,
    ) -> Result<(SignedReceipt, LedgerState)> {
        let sequence = state.last_sequence + 1;
        let receipt = SignedReceipt::sign(
            ReceiptBody {
                schema: RECEIPT_SCHEMA.to_owned(),
                sequence,
                previous: state.head.clone(),
                node_fingerprint: self.node.fingerprint().to_owned(),
                request_digest: draft.request.digest()?,
                request: draft.request,
                measured_preconditions: draft.measured_preconditions,
                result: draft.result,
                discarded_grant_digests: draft.discarded_grant_digests,
                recorded_at: lillux::time::iso8601_now(),
            },
            self.node.signing_key(),
        )?;
        self.chain.write_entry(sequence, &receipt)?;
        let mut next = state.clone();
        next.head = receipt.digest()?;
        next.last_sequence = sequence;
        next.request_digests
            .insert(receipt.body.request_digest.clone());
        next.past_heads.insert(next.head.clone());
        next.retained += 1;
        if next.oldest_recorded_at.is_none() {
            next.oldest_recorded_at = Some(receipt.body.recorded_at.clone());
        }
        if next.retained > LEDGER_ROTATE_AT {
            self.chain.rotate()?;
            return Ok((receipt, self.load()?));
        }
        Ok((receipt, next))
    }
}

/// What the application records about one applied request.
pub(super) struct ReceiptDraft {
    pub request: SignedRequest,
    pub measured_preconditions: std::collections::BTreeMap<String, String>,
    pub result: Value,
    pub discarded_grant_digests: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::offline_operation::contract::{REQUEST_SCHEMA, RequestBody};

    struct Fixture {
        root: tempfile::TempDir,
        node: NodeIdentity,
        store: Store,
    }

    fn fixture() -> Fixture {
        let root = tempfile::tempdir().unwrap();
        let node =
            NodeIdentity::create(&root.path().join(".ai/node/identity/private_key.pem")).unwrap();
        let store = Store::for_app_root(root.path());
        store.create_layout().unwrap();
        Fixture { root, node, store }
    }

    fn draft(node: &NodeIdentity, tag: u8) -> ReceiptDraft {
        let key = lillux::crypto::generate_signing_key();
        let request = SignedRequest::sign(
            RequestBody {
                schema: REQUEST_SCHEMA.to_owned(),
                node_fingerprint: node.fingerprint().to_owned(),
                signer_fingerprint: "b".repeat(64),
                signer_grant_digest: None,
                signer_origin_site_id: None,
                operation: "node.allow-restart".to_owned(),
                arguments: serde_json::json!({}),
                inputs: Default::default(),
                expected_result: Default::default(),
                precondition: Default::default(),
                issued_at: format!("2026-10-28T00:00:{tag:02}Z"),
                not_after: "2026-10-28T01:00:00Z".to_owned(),
            },
            &key,
        )
        .unwrap();
        ReceiptDraft {
            request,
            measured_preconditions: Default::default(),
            result: serde_json::json!({"tag": tag}),
            discarded_grant_digests: Vec::new(),
        }
    }

    #[test]
    fn an_empty_ledger_has_the_genesis_head() {
        let fx = fixture();
        let state = Ledger::new(&fx.store, &fx.node).load().unwrap();
        assert_eq!(state.head, genesis_ledger_head(fx.node.fingerprint()));
        assert_eq!(state.last_sequence, 0);
    }

    #[test]
    fn receipts_chain_and_the_head_follows_the_last() {
        let fx = fixture();
        let ledger = Ledger::new(&fx.store, &fx.node);
        let mut state = ledger.load().unwrap();
        let genesis = state.head.clone();
        let (first, next) = ledger.append(&state, draft(&fx.node, 1)).unwrap();
        assert_eq!(first.body.previous, genesis);
        state = next;
        let (second, next) = ledger.append(&state, draft(&fx.node, 2)).unwrap();
        assert_eq!(second.body.previous, first.digest().unwrap());
        assert_eq!(next.head, second.digest().unwrap());
        let reloaded = ledger.load().unwrap();
        assert_eq!(reloaded, next);
        assert!(reloaded.is_past_head(&first.digest().unwrap()));
        assert!(!reloaded.is_past_head(&reloaded.head));
        assert!(!reloaded.is_past_head(&"9".repeat(64)));
    }

    #[test]
    fn a_receipt_cannot_be_written_over() {
        let fx = fixture();
        let ledger = Ledger::new(&fx.store, &fx.node);
        let state = ledger.load().unwrap();
        ledger.append(&state, draft(&fx.node, 1)).unwrap();
        // The stale state would reuse sequence 1.
        assert!(ledger.append(&state, draft(&fx.node, 2)).is_err());
    }

    #[test]
    fn a_tampered_receipt_breaks_the_chain() {
        let fx = fixture();
        let ledger = Ledger::new(&fx.store, &fx.node);
        let mut state = ledger.load().unwrap();
        for tag in 1..=3 {
            state = ledger.append(&state, draft(&fx.node, tag)).unwrap().1;
        }
        let path = fx.store.ledger_dir().join(receipt_name(2));
        let mut receipt: SignedReceipt =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        receipt.body.result = serde_json::json!({"forged": true});
        std::fs::write(&path, serde_json::to_vec(&receipt).unwrap()).unwrap();
        assert!(ledger.load().is_err());
    }

    #[test]
    fn a_removed_receipt_breaks_the_chain() {
        let fx = fixture();
        let ledger = Ledger::new(&fx.store, &fx.node);
        let mut state = ledger.load().unwrap();
        for tag in 1..=3 {
            state = ledger.append(&state, draft(&fx.node, tag)).unwrap().1;
        }
        std::fs::remove_file(fx.store.ledger_dir().join(receipt_name(2))).unwrap();
        assert!(ledger.load().is_err());
    }

    #[test]
    fn a_receipt_signed_by_another_key_is_refused() {
        let fx = fixture();
        let ledger = Ledger::new(&fx.store, &fx.node);
        let state = ledger.load().unwrap();
        ledger.append(&state, draft(&fx.node, 1)).unwrap();
        let other = NodeIdentity::create(&fx.root.path().join("other.pem")).unwrap();
        let path = fx.store.ledger_dir().join(receipt_name(1));
        let mut receipt: SignedReceipt =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        receipt = SignedReceipt::sign(receipt.body, other.signing_key()).unwrap();
        std::fs::write(&path, serde_json::to_vec(&receipt).unwrap()).unwrap();
        assert!(ledger.load().is_err());
    }

    #[test]
    fn rotation_carries_the_tip_in_a_checkpoint() {
        let fx = fixture();
        let ledger = Ledger::new(&fx.store, &fx.node);
        let mut state = ledger.load().unwrap();
        let mut heads = Vec::new();
        for tag in 0..=LEDGER_ROTATE_AT {
            state = ledger
                .append(&state, draft(&fx.node, u8::try_from(tag % 50).unwrap()))
                .unwrap()
                .1;
            heads.push(state.head.clone());
        }
        let loaded = ledger.load().unwrap();
        assert_eq!(loaded.head, *heads.last().unwrap());
        assert_eq!(
            loaded.last_sequence,
            u64::try_from(LEDGER_ROTATE_AT).unwrap() + 1
        );
        assert!(loaded.retained <= LEDGER_KEEP + 1);
        assert!(loaded.dropped_receipts > 0);
        assert!(fx.store.ledger_dir().join(CHECKPOINT_FILE).exists());
        // A request issued before the retained receipts and pinned to a head
        // the ledger does not hold names a head rotation dropped.
        assert!(loaded.predates_retained(hour_ago()));
        assert!(!loaded.predates_retained(hour_ahead()));
        // Appending continues the chain across the checkpoint.
        let (next, _) = ledger.append(&loaded, draft(&fx.node, 7)).unwrap();
        assert_eq!(next.body.previous, loaded.head);
        ledger.load().unwrap();
    }

    fn hour_ago() -> CanonicalInstant {
        CanonicalInstant::from_unix_secs(lillux::time::unix_secs_now() - 3600)
    }

    fn hour_ahead() -> CanonicalInstant {
        CanonicalInstant::from_unix_secs(lillux::time::unix_secs_now() + 3600)
    }

    #[test]
    fn a_ledger_that_never_rotated_drops_no_head() {
        let fx = fixture();
        let ledger = Ledger::new(&fx.store, &fx.node);
        let state = ledger
            .append(&ledger.load().unwrap(), draft(&fx.node, 1))
            .unwrap()
            .1;
        assert!(!state.predates_retained(hour_ago()));
    }

    #[test]
    fn a_stray_file_does_not_stop_the_ledger() {
        let fx = fixture();
        let ledger = Ledger::new(&fx.store, &fx.node);
        std::fs::write(fx.store.ledger_dir().join("notes.txt"), b"hello").unwrap();
        std::fs::write(
            fx.store.ledger_dir().join("00000000000000000001.json.tmp"),
            b"{}",
        )
        .unwrap();
        let state = ledger.load().unwrap();
        let (_, next) = ledger.append(&state, draft(&fx.node, 1)).unwrap();
        assert_eq!(ledger.load().unwrap(), next);
    }

    #[test]
    fn a_receipt_name_that_is_not_a_file_is_a_specific_error() {
        let fx = fixture();
        let ledger = Ledger::new(&fx.store, &fx.node);
        std::fs::create_dir(fx.store.ledger_dir().join("00000000000000000001.json")).unwrap();
        let error = format!("{:#}", ledger.load().unwrap_err());
        assert!(error.contains("is not a regular file"), "{error}");
        assert!(error.contains("00000000000000000001.json"), "{error}");
    }
}
