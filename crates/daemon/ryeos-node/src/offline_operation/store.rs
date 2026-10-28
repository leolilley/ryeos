//! The on-volume layout of offline operations and the bounded reads and
//! durable writes around it.
//!
//! Everything lives under `<app_root>/.ai/state/offline-operations/`, in
//! directories owned by the node account with mode 0700:
//!
//! - `inbox/`: `<64-hex>.json` requests and `<64-hex>.input` inputs, written
//!   by others and read here; anything else is ignored.
//! - `refused/`: node-signed refusal receipts and the refused request copies.
//! - `pending/`: one intent record per request being applied.
//! - `ledger/`: the hash-chained receipts; written only by the application.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use lillux::PinnedDirectory;
use ryeos_engine::roots::RuntimeRoot;
use ryeos_node_runtime::identity::NodeIdentity;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::contract::{
    MAX_INBOX_REQUESTS, MAX_REQUEST_BYTES, REFUSAL_SCHEMA, Refusal, RefusalBody, RefusalReason,
    SignedRefusal, SignedRequest, is_digest,
};

pub const DIRECTORY: &str = "offline-operations";
const INBOX: &str = "inbox";
const REFUSED: &str = "refused";
const PENDING: &str = "pending";
const LEDGER: &str = "ledger";

/// Most refusal receipts kept; the oldest are pruned past it.
pub const MAX_REFUSAL_RECEIPTS: usize = 64;
/// Most bytes the refused directory holds before the oldest are pruned.
pub const MAX_REFUSED_BYTES: u64 = 2 * 1024 * 1024;
const MAX_REFUSAL_RECEIPT_BYTES: u64 = 16 * 1024;
const MAX_PENDING_BYTES: u64 = 1024 * 1024;
/// How many inbox names are read in one listing, to bound the work a flooded
/// inbox can cost one startup.
const MAX_INBOX_LISTING: usize = 4096;

#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

/// A bounded read of one file.
#[derive(Debug)]
pub enum Read {
    Missing,
    TooLarge,
    Bytes(Vec<u8>),
}

/// The names in the inbox that the application reads.
#[derive(Debug, Default)]
pub struct Inbox {
    /// Request digests (the names' stems), sorted, at most the inbox bound.
    pub requests: Vec<String>,
    /// More requests than the bound were present; the rest wait.
    pub overflow: bool,
}

/// Where in its application a request stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// The handler started. A record found in this phase at startup means
    /// the node died inside the handler or the handler failed.
    Running,
    /// The handler finished; the receipt is not yet in the ledger.
    Committed,
    /// The receipt is in the ledger but the effect is not yet complete
    /// (the receipt-first operations).
    Receipted,
}

/// The intent record of one request being applied.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pending {
    pub request_digest: String,
    /// The name of the request file in the inbox, which names the digest of
    /// its bytes as written (not of its canonical body).
    pub inbox_name: String,
    /// The request itself, so a continuation can finish and receipt it even
    /// if its file has gone.
    pub request: SignedRequest,
    /// Startups that have begun applying this request.
    pub attempts: u32,
    pub phase: Phase,
    pub measured_preconditions: std::collections::BTreeMap<String, String>,
    pub result: Option<Value>,
    pub discarded_grant_digests: Vec<String>,
    pub started_at: String,
}

impl Store {
    pub fn for_app_root(app_root: &Path) -> Self {
        Self {
            root: RuntimeRoot::new(app_root.to_path_buf())
                .state()
                .join(DIRECTORY),
        }
    }

    pub fn inbox_dir(&self) -> PathBuf {
        self.root.join(INBOX)
    }

    pub fn refused_dir(&self) -> PathBuf {
        self.root.join(REFUSED)
    }

    pub fn pending_dir(&self) -> PathBuf {
        self.root.join(PENDING)
    }

    pub fn ledger_dir(&self) -> PathBuf {
        self.root.join(LEDGER)
    }

    /// Create the directories, owner-private. Idempotent.
    pub fn create_layout(&self) -> Result<()> {
        let root = PinnedDirectory::open_or_create(&self.root)?;
        root.tighten_owner_private_directory()?;
        for name in [INBOX, REFUSED, PENDING, LEDGER] {
            root.open_or_create_child(OsStr::new(name), 0o700)?
                .tighten_owner_private_directory()?;
        }
        Ok(())
    }

    // ── Inbox ────────────────────────────────────────────────────────────

    pub fn list_inbox(&self) -> Result<Inbox> {
        let Some(directory) = PinnedDirectory::open(&self.inbox_dir())? else {
            return Ok(Inbox::default());
        };
        let entries = directory.entries_no_follow_bounded(MAX_INBOX_LISTING)?;
        let mut requests: Vec<String> = entries
            .iter()
            .filter(|entry| entry.entry_type == lillux::PinnedEntryType::Regular)
            .filter_map(|entry| entry.name.to_str())
            .filter_map(|name| name.strip_suffix(".json"))
            .filter(|stem| is_digest(stem))
            .map(str::to_owned)
            .collect();
        requests.sort();
        let overflow = requests.len() > MAX_INBOX_REQUESTS;
        requests.truncate(MAX_INBOX_REQUESTS);
        Ok(Inbox { requests, overflow })
    }

    pub fn read_request(&self, digest: &str) -> Result<Read> {
        read_bounded(
            &self.inbox_dir(),
            &format!("{digest}.json"),
            MAX_REQUEST_BYTES,
        )
    }

    /// Read one input: opened once, its size checked against the slot's
    /// bound before any byte is read, and the bytes returned from that same
    /// descriptor. The caller compares their digest with the pinned one.
    pub fn read_input(&self, digest: &str, max_bytes: u64) -> Result<Read> {
        read_bounded(&self.inbox_dir(), &format!("{digest}.input"), max_bytes)
    }

    pub fn remove_request(&self, digest: &str) -> Result<()> {
        remove(&self.inbox_dir().join(format!("{digest}.json")))
    }

    pub fn remove_input(&self, digest: &str) -> Result<()> {
        remove(&self.inbox_dir().join(format!("{digest}.input")))
    }

    // ── Pending intent records ───────────────────────────────────────────

    fn pending_path(&self, digest: &str) -> PathBuf {
        self.pending_dir().join(format!("{digest}.json"))
    }

    pub fn load_pending(&self, digest: &str) -> Result<Option<Pending>> {
        let path = self.pending_path(digest);
        let Some(bytes) =
            lillux::read_optional_regular_file_bounded_no_follow(&path, MAX_PENDING_BYTES)?
        else {
            return Ok(None);
        };
        let pending: Pending = serde_json::from_slice(&bytes)
            .with_context(|| format!("parse intent record {}", path.display()))?;
        ensure!(
            pending.request_digest == digest,
            "intent record {} names another request",
            path.display()
        );
        Ok(Some(pending))
    }

    pub fn save_pending(&self, pending: &Pending) -> Result<()> {
        write_private(
            &self.pending_path(&pending.request_digest),
            &serde_json::to_vec(pending)?,
        )
    }

    pub fn remove_pending(&self, digest: &str) -> Result<()> {
        remove(&self.pending_path(digest))
    }

    /// Digests of every request that has an intent record.
    pub fn pending_digests(&self) -> Result<Vec<String>> {
        let Some(directory) = PinnedDirectory::open(&self.pending_dir())? else {
            return Ok(Vec::new());
        };
        let mut digests: Vec<String> = directory
            .entries_no_follow_bounded(MAX_INBOX_LISTING)?
            .iter()
            .filter_map(|entry| entry.name.to_str())
            .filter_map(|name| name.strip_suffix(".json"))
            .filter(|stem| is_digest(stem))
            .map(str::to_owned)
            .collect();
        digests.sort();
        Ok(digests)
    }

    // ── Refusals ─────────────────────────────────────────────────────────

    fn refusal_path(&self, digest: &str, reason: RefusalReason) -> PathBuf {
        self.refused_dir()
            .join(format!("{digest}.{}.refusal.json", reason.code()))
    }

    fn refused_request_path(&self, digest: &str) -> PathBuf {
        self.refused_dir().join(format!("{digest}.request"))
    }

    /// Record a refusal: one receipt per (request, reason), whose count rises
    /// on repeats; keep a copy of the refused request when it fits; prune the
    /// directory to its bounds.
    pub fn record_refusal(
        &self,
        node: &NodeIdentity,
        request_digest: &str,
        operation: Option<&str>,
        signer_fingerprint: Option<&str>,
        refusal: &Refusal,
        request_bytes: Option<&[u8]>,
    ) -> Result<SignedRefusal> {
        ensure!(is_digest(request_digest), "refusal names no digest");
        let now = lillux::time::iso8601_now();
        let path = self.refusal_path(request_digest, refusal.reason);
        let (occurrences, first_seen) = match self.read_refusal(&path)? {
            Some(previous) => (previous.body.occurrences + 1, previous.body.first_seen),
            None => (1, now.clone()),
        };
        let receipt = SignedRefusal::sign(
            RefusalBody {
                schema: REFUSAL_SCHEMA.to_owned(),
                node_fingerprint: node.fingerprint().to_owned(),
                request_digest: request_digest.to_owned(),
                operation: operation.map(str::to_owned),
                signer_fingerprint: signer_fingerprint.map(str::to_owned),
                reason: refusal.reason,
                detail: refusal.detail.chars().take(1024).collect(),
                occurrences,
                first_seen,
                last_seen: now,
            },
            node.signing_key(),
        )?;
        write_private(&path, &serde_json::to_vec(&receipt)?)?;
        if let Some(bytes) = request_bytes {
            let copy = self.refused_request_path(request_digest);
            if u64::try_from(bytes.len())? <= MAX_REQUEST_BYTES
                && lillux::inspect_optional_entry_no_follow(&copy)?.is_none()
            {
                write_private(&copy, bytes)?;
            }
        }
        self.prune_refused()?;
        Ok(receipt)
    }

    fn read_refusal(&self, path: &Path) -> Result<Option<SignedRefusal>> {
        let Some(bytes) =
            lillux::read_optional_regular_file_bounded_no_follow(path, MAX_REFUSAL_RECEIPT_BYTES)?
        else {
            return Ok(None);
        };
        Ok(Some(serde_json::from_slice(&bytes).with_context(|| {
            format!("parse refusal receipt {}", path.display())
        })?))
    }

    /// Every refusal receipt, oldest first by last sighting.
    pub fn refusals(&self) -> Result<Vec<SignedRefusal>> {
        let Some(directory) = PinnedDirectory::open(&self.refused_dir())? else {
            return Ok(Vec::new());
        };
        let mut receipts = Vec::new();
        for entry in directory.entries_no_follow_bounded(MAX_INBOX_LISTING)? {
            let Some(name) = entry.name.to_str() else {
                continue;
            };
            if !name.ends_with(".refusal.json") {
                continue;
            }
            if let Some(receipt) = self.read_refusal(&self.refused_dir().join(name))? {
                receipts.push(receipt);
            }
        }
        receipts.sort_by(|a, b| {
            a.body
                .last_seen
                .cmp(&b.body.last_seen)
                .then_with(|| a.body.request_digest.cmp(&b.body.request_digest))
        });
        Ok(receipts)
    }

    fn prune_refused(&self) -> Result<()> {
        let receipts = self.refusals()?;
        let mut bytes: u64 = 0;
        if let Some(directory) = PinnedDirectory::open(&self.refused_dir())? {
            for entry in directory.entries_no_follow_bounded(MAX_INBOX_LISTING)? {
                if entry.entry_type == lillux::PinnedEntryType::Regular
                    && let Some(file) = directory.open_regular(&entry.name, false)?
                {
                    bytes = bytes.saturating_add(file.metadata()?.len());
                }
            }
        }
        let mut count = receipts.len();
        for receipt in receipts {
            if count <= MAX_REFUSAL_RECEIPTS && bytes <= MAX_REFUSED_BYTES {
                break;
            }
            let digest = &receipt.body.request_digest;
            let receipt_path = self.refusal_path(digest, receipt.body.reason);
            bytes = bytes.saturating_sub(file_len(&receipt_path));
            remove(&receipt_path)?;
            count -= 1;
            // The request copy goes with the last receipt that names it.
            let others = self
                .refusals()?
                .iter()
                .any(|other| other.body.request_digest == *digest);
            if !others {
                let copy = self.refused_request_path(digest);
                bytes = bytes.saturating_sub(file_len(&copy));
                remove(&copy)?;
            }
        }
        Ok(())
    }
}

fn file_len(path: &Path) -> u64 {
    lillux::read_optional_regular_file_bounded_no_follow(path, u64::MAX)
        .ok()
        .flatten()
        .map_or(0, |bytes| u64::try_from(bytes.len()).unwrap_or(0))
}

fn read_bounded(directory: &Path, name: &str, max_bytes: u64) -> Result<Read> {
    let Some(pinned) = PinnedDirectory::open(directory)? else {
        return Ok(Read::Missing);
    };
    let Some(file) = pinned.open_regular(OsStr::new(name), false)? else {
        return Ok(Read::Missing);
    };
    let observation = lillux::observe_open_regular_file(&file)?;
    if observation.size() > max_bytes {
        return Ok(Read::TooLarge);
    }
    Ok(Read::Bytes(lillux::read_open_regular_file_stable_bounded(
        &file,
        &observation,
        max_bytes,
    )?))
}

/// Durably write a file owner-private, replacing any earlier content.
pub(super) fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    lillux::atomic_write_private(path, bytes)
        .map_err(anyhow::Error::from)
        .with_context(|| format!("write {}", path.display()))
}

/// Durably remove a file; absent is already removed.
pub(super) fn remove(path: &Path) -> Result<()> {
    lillux::remove_file_durable(path)
        .map_err(anyhow::Error::from)
        .with_context(|| format!("remove {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(root: &Path) -> NodeIdentity {
        NodeIdentity::create(&root.join(".ai/node/identity/private_key.pem")).unwrap()
    }

    fn refusal(reason: RefusalReason) -> Refusal {
        Refusal::new(reason, "detail")
    }

    #[test]
    fn only_digest_named_json_files_are_requests() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::for_app_root(root.path());
        store.create_layout().unwrap();
        let inbox = store.inbox_dir();
        std::fs::write(inbox.join(format!("{}.json", "a".repeat(64))), b"{}").unwrap();
        std::fs::write(inbox.join(format!("{}.input", "b".repeat(64))), b"x").unwrap();
        std::fs::write(inbox.join("notes.json"), b"{}").unwrap();
        std::fs::write(inbox.join(format!("{}.json.tmp", "c".repeat(64))), b"{}").unwrap();
        std::fs::create_dir(inbox.join(format!("{}.json", "d".repeat(64)))).unwrap();
        let listing = store.list_inbox().unwrap();
        assert_eq!(listing.requests, ["a".repeat(64)]);
        assert!(!listing.overflow);
    }

    #[test]
    fn the_inbox_holds_sixteen_requests_and_reports_the_rest() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::for_app_root(root.path());
        store.create_layout().unwrap();
        for index in 0..20_u8 {
            std::fs::write(
                store.inbox_dir().join(format!("{:064x}.json", index)),
                b"{}",
            )
            .unwrap();
        }
        let listing = store.list_inbox().unwrap();
        assert_eq!(listing.requests.len(), MAX_INBOX_REQUESTS);
        assert!(listing.overflow);
    }

    #[test]
    fn an_oversized_file_is_reported_without_being_read() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::for_app_root(root.path());
        store.create_layout().unwrap();
        let digest = "e".repeat(64);
        std::fs::write(
            store.inbox_dir().join(format!("{digest}.json")),
            vec![b' '; usize::try_from(MAX_REQUEST_BYTES).unwrap() + 1],
        )
        .unwrap();
        assert!(matches!(
            store.read_request(&digest).unwrap(),
            Read::TooLarge
        ));
        assert!(matches!(
            store.read_request(&"f".repeat(64)).unwrap(),
            Read::Missing
        ));
    }

    #[test]
    fn repeated_refusals_raise_a_count_instead_of_adding_receipts() {
        let root = tempfile::tempdir().unwrap();
        let identity = node(root.path());
        let store = Store::for_app_root(root.path());
        store.create_layout().unwrap();
        let digest = "1".repeat(64);
        for _ in 0..3 {
            store
                .record_refusal(
                    &identity,
                    &digest,
                    None,
                    None,
                    &refusal(RefusalReason::BadSignature),
                    Some(b"{}"),
                )
                .unwrap();
        }
        store
            .record_refusal(
                &identity,
                &digest,
                None,
                None,
                &refusal(RefusalReason::Expired),
                None,
            )
            .unwrap();
        let receipts = store.refusals().unwrap();
        assert_eq!(receipts.len(), 2);
        let bad = receipts
            .iter()
            .find(|r| r.body.reason == RefusalReason::BadSignature)
            .unwrap();
        assert_eq!(bad.body.occurrences, 3);
        bad.verify(identity.verifying_key()).unwrap();
    }

    #[test]
    fn the_refused_directory_is_bounded_by_count() {
        let root = tempfile::tempdir().unwrap();
        let identity = node(root.path());
        let store = Store::for_app_root(root.path());
        store.create_layout().unwrap();
        for index in 0..(MAX_REFUSAL_RECEIPTS + 10) {
            store
                .record_refusal(
                    &identity,
                    &format!("{index:064x}"),
                    None,
                    None,
                    &refusal(RefusalReason::Malformed),
                    Some(b"{}"),
                )
                .unwrap();
        }
        assert_eq!(store.refusals().unwrap().len(), MAX_REFUSAL_RECEIPTS);
        // The newest survive; the request copies of pruned receipts go too.
        let newest = format!("{:064x}", MAX_REFUSAL_RECEIPTS + 9);
        assert!(store.refused_request_path(&newest).exists());
        assert!(!store.refused_request_path(&format!("{:064x}", 0)).exists());
    }

    #[test]
    fn pending_records_round_trip_and_name_their_request() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::for_app_root(root.path());
        store.create_layout().unwrap();
        let key = lillux::crypto::generate_signing_key();
        let request = SignedRequest::sign(
            crate::offline_operation::contract::RequestBody {
                schema: crate::offline_operation::contract::REQUEST_SCHEMA.to_owned(),
                node_fingerprint: "a".repeat(64),
                signer_fingerprint: "b".repeat(64),
                signer_grant_digest: None,
                signer_origin_site_id: None,
                operation: "node.allow-restart".to_owned(),
                arguments: serde_json::json!({}),
                inputs: Default::default(),
                expected_result: Default::default(),
                precondition: Default::default(),
                issued_at: "2026-10-28T00:00:00Z".to_owned(),
                not_after: "2026-10-28T01:00:00Z".to_owned(),
            },
            &key,
        )
        .unwrap();
        let pending = Pending {
            request_digest: "2".repeat(64),
            inbox_name: "3".repeat(64),
            request,
            attempts: 1,
            phase: Phase::Running,
            measured_preconditions: Default::default(),
            result: None,
            discarded_grant_digests: Vec::new(),
            started_at: "2026-10-28T00:00:00Z".to_owned(),
        };
        store.save_pending(&pending).unwrap();
        assert_eq!(
            store.load_pending(&pending.request_digest).unwrap(),
            Some(pending.clone())
        );
        assert_eq!(
            store.pending_digests().unwrap(),
            std::slice::from_ref(&pending.request_digest)
        );
        store.remove_pending(&pending.request_digest).unwrap();
        assert_eq!(store.load_pending(&pending.request_digest).unwrap(), None);
    }
}
