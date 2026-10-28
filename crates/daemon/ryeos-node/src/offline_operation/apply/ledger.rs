//! The hash-chained, append-only ledger of applied operations.
//!
//! Each receipt carries the digest of the one before it, and is signed with
//! the node key when it is written. The ledger lives in a directory only the
//! application writes: this module is a child of the application so that its
//! writing half is invisible to the handlers and to every reset. Reading is
//! open to anything that wants the chain tip.

use std::collections::BTreeSet;
use std::path::PathBuf;

use anyhow::{Context, Result, ensure};
use ryeos_node_runtime::identity::NodeIdentity;
use serde_json::Value;

use crate::offline_operation::contract::{
    CHECKPOINT_SCHEMA, CheckpointBody, MAX_RECEIPT_BYTES, RECEIPT_SCHEMA, ReceiptBody,
    SignedCheckpoint, SignedReceipt, SignedRequest, genesis_ledger_head,
};
use crate::offline_operation::store::{Store, bounded_names, remove, write_private};

/// Receipts kept before rotation drops the oldest into a checkpoint.
pub const LEDGER_ROTATE_AT: usize = 1024;
/// Receipts kept after a rotation.
pub const LEDGER_KEEP: usize = 512;

const CHECKPOINT_FILE: &str = "checkpoint.json";

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
    pub fn predates_retained(&self, issued_at: chrono::DateTime<chrono::Utc>) -> bool {
        let Some(checkpoint) = &self.checkpoint_recorded_at else {
            return false;
        };
        let reference = self.oldest_recorded_at.as_ref().unwrap_or(checkpoint);
        crate::offline_operation::contract::parse_instant("recorded_at", reference)
            .is_ok_and(|reference| issued_at <= reference)
    }

    /// Whether `head` was once this ledger's head and no longer is.
    pub fn is_past_head(&self, head: &str) -> bool {
        head != self.head && self.past_heads.contains(head)
    }
}

pub struct Ledger<'a> {
    dir: PathBuf,
    node: &'a NodeIdentity,
}

fn receipt_name(sequence: u64) -> String {
    format!("{sequence:020}.json")
}

impl<'a> Ledger<'a> {
    pub fn new(store: &Store, node: &'a NodeIdentity) -> Self {
        Self {
            dir: store.ledger_dir(),
            node,
        }
    }

    fn checkpoint(&self) -> Result<Option<SignedCheckpoint>> {
        let path = self.dir.join(CHECKPOINT_FILE);
        let Some(bytes) =
            lillux::read_optional_regular_file_bounded_no_follow(&path, MAX_RECEIPT_BYTES)?
        else {
            return Ok(None);
        };
        let checkpoint: SignedCheckpoint =
            serde_json::from_slice(&bytes).context("parse ledger checkpoint")?;
        checkpoint.verify(self.node.verifying_key())?;
        ensure!(
            checkpoint.body.schema == CHECKPOINT_SCHEMA
                && checkpoint.body.node_fingerprint == self.node.fingerprint(),
            "ledger checkpoint is not this node's"
        );
        Ok(Some(checkpoint))
    }

    /// The sequences of the receipts present. Names that are not receipts
    /// are ignored (a stray file does not stop the node's repair); a name that
    /// is a receipt's but is not a regular file is an error naming it, and a
    /// directory too full to list whole is an error, since a partial list
    /// would read as a broken chain.
    fn sequences(&self) -> Result<Vec<u64>> {
        let names = bounded_names(&self.dir, LEDGER_ROTATE_AT * 4)
            .with_context(|| format!("list the offline-operation ledger {}", self.dir.display()))?;
        ensure!(
            !names.truncated,
            "the offline-operation ledger {} holds more than {} entries",
            self.dir.display(),
            LEDGER_ROTATE_AT * 4
        );
        let mut sequences = Vec::new();
        for (name, regular) in names.entries {
            let Some(stem) = name
                .strip_suffix(".json")
                .filter(|stem| stem.len() == 20 && stem.bytes().all(|b| b.is_ascii_digit()))
            else {
                continue;
            };
            ensure!(
                regular,
                "ledger entry {} is not a regular file",
                self.dir.join(&name).display()
            );
            sequences.push(stem.parse::<u64>()?);
        }
        sequences.sort_unstable();
        Ok(sequences)
    }

    fn read_receipt(&self, sequence: u64) -> Result<SignedReceipt> {
        let path = self.dir.join(receipt_name(sequence));
        let bytes = lillux::read_regular_file_bounded_no_follow(&path, MAX_RECEIPT_BYTES)?;
        serde_json::from_slice(&bytes).with_context(|| format!("parse receipt {}", path.display()))
    }

    /// Read and verify the whole retained chain: signatures, sequence,
    /// links, and the node each receipt names.
    pub fn load(&self) -> Result<LedgerState> {
        let checkpoint = self.checkpoint()?;
        let (mut head, mut last_sequence, dropped) = match &checkpoint {
            Some(checkpoint) => (
                checkpoint.body.tip.clone(),
                checkpoint.body.sequence,
                checkpoint.body.dropped_receipts,
            ),
            None => (genesis_ledger_head(self.node.fingerprint()), 0, 0),
        };
        // The starting head (genesis, or a checkpoint's tip) is a head the
        // ledger has been at: a request pinned to it is stale once a receipt
        // follows. The current head itself is never "past" (`is_past_head`).
        let mut past_heads = BTreeSet::from([head.clone()]);
        let mut request_digests = BTreeSet::new();
        let mut retained = 0;
        let mut oldest_recorded_at = None;
        for sequence in self.sequences()? {
            if sequence <= last_sequence {
                // Already covered by the checkpoint: a rotation that died
                // before removing it.
                continue;
            }
            ensure!(
                sequence == last_sequence + 1,
                "ledger skips from receipt {last_sequence} to {sequence}"
            );
            let receipt = self.read_receipt(sequence)?;
            receipt.verify(self.node.verifying_key())?;
            let body = &receipt.body;
            ensure!(
                body.schema == RECEIPT_SCHEMA
                    && body.sequence == sequence
                    && body.node_fingerprint == self.node.fingerprint(),
                "receipt {sequence} is not this ledger's"
            );
            ensure!(
                body.previous == head,
                "receipt {sequence} does not continue the chain"
            );
            if oldest_recorded_at.is_none() {
                oldest_recorded_at = Some(body.recorded_at.clone());
            }
            head = receipt.digest()?;
            past_heads.insert(head.clone());
            request_digests.insert(body.request_digest.clone());
            last_sequence = sequence;
            retained += 1;
        }
        Ok(LedgerState {
            head,
            last_sequence,
            request_digests,
            past_heads,
            retained,
            dropped_receipts: dropped,
            oldest_recorded_at,
            checkpoint_recorded_at: checkpoint.map(|checkpoint| checkpoint.body.recorded_at),
        })
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
        let mut sequences = self.sequences()?;
        sequences.reverse();
        for sequence in sequences {
            if sequence > state.last_sequence {
                continue;
            }
            let receipt = self.read_receipt(sequence)?;
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
        let path = self.dir.join(receipt_name(sequence));
        ensure!(
            lillux::inspect_optional_entry_no_follow(&path)?.is_none(),
            "receipt {sequence} already exists; the ledger will not overwrite it"
        );
        write_private(&path, &serde_json::to_vec(&receipt)?)?;
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
            self.rotate()?;
            return Ok((receipt, self.load()?));
        }
        Ok((receipt, next))
    }

    /// Drop the oldest receipts into a signed checkpoint that carries the
    /// chain tip forward. The checkpoint is durable before anything is
    /// removed, so a death in between leaves a ledger that still verifies.
    fn rotate(&self) -> Result<()> {
        let sequences = self.sequences()?;
        let checkpoint = self.checkpoint()?;
        let covered = checkpoint.as_ref().map_or(0, |c| c.body.sequence);
        let live: Vec<u64> = sequences.into_iter().filter(|s| *s > covered).collect();
        if live.len() <= LEDGER_KEEP {
            return Ok(());
        }
        let drop_through = live[live.len() - LEDGER_KEEP - 1];
        let tip = self.read_receipt(drop_through)?.digest()?;
        let dropped_now = live.iter().filter(|s| **s <= drop_through).count() as u64;
        let signed = SignedCheckpoint::sign(
            CheckpointBody {
                schema: CHECKPOINT_SCHEMA.to_owned(),
                node_fingerprint: self.node.fingerprint().to_owned(),
                sequence: drop_through,
                tip,
                dropped_receipts: checkpoint.as_ref().map_or(0, |c| c.body.dropped_receipts)
                    + dropped_now,
                recorded_at: lillux::time::iso8601_now(),
            },
            self.node.signing_key(),
        )?;
        write_private(
            &self.dir.join(CHECKPOINT_FILE),
            &serde_json::to_vec(&signed)?,
        )?;
        for sequence in live.into_iter().filter(|s| *s <= drop_through) {
            remove(&self.dir.join(receipt_name(sequence)))?;
        }
        Ok(())
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
        assert!(loaded.predates_retained(chrono::Utc::now() - chrono::Duration::hours(1)));
        assert!(!loaded.predates_retained(chrono::Utc::now() + chrono::Duration::hours(1)));
        // Appending continues the chain across the checkpoint.
        let (next, _) = ledger.append(&loaded, draft(&fx.node, 7)).unwrap();
        assert_eq!(next.body.previous, loaded.head);
        ledger.load().unwrap();
    }

    #[test]
    fn a_ledger_that_never_rotated_drops_no_head() {
        let fx = fixture();
        let ledger = Ledger::new(&fx.store, &fx.node);
        let state = ledger
            .append(&ledger.load().unwrap(), draft(&fx.node, 1))
            .unwrap()
            .1;
        assert!(!state.predates_retained(chrono::Utc::now() - chrono::Duration::hours(1)));
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
