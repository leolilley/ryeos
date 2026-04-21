//! Chain operations — create, append, read, lock.
//!
//! Every execution chain is a hash-linked sequence of immutable CAS objects.
//! All writes are protected by file locks and validated against the signed head ref.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::path::Path;

use anyhow::{anyhow, Context};

use crate::objects::{ChainState, ChainThreadEntry, ThreadEvent, ThreadSnapshot};
use crate::signer::Signer;
use crate::{refs, HeadCache, SignedRef};

pub use self::types::*;

mod types {
    use crate::objects::{ThreadEvent, ThreadSnapshot};

    /// Result of creating a new execution chain.
    #[derive(Debug, Clone)]
    pub struct CreateResult {
        /// CAS hash of the initial chain state.
        pub chain_state_hash: String,
        /// CAS hash of the initial thread snapshot.
        pub snapshot_hash: String,
        /// The events that were persisted.
        pub events: Vec<ThreadEvent>,
    }

    /// Result of appending events to a chain.
    #[derive(Debug, Clone)]
    pub struct AppendResult {
        /// CAS hash of the new chain state.
        pub chain_state_hash: String,
        /// Sequence number of the first appended event.
        pub first_chain_seq: u64,
        /// Number of events appended.
        pub event_count: usize,
        /// The events that were persisted.
        pub events: Vec<ThreadEvent>,
    }

    /// Result of reading a chain snapshot.
    #[derive(Debug, Clone)]
    pub struct ReadSnapshotResult {
        /// The thread snapshot, if found.
        pub snapshot: Option<ThreadSnapshot>,
        /// The chain state hash at the time of read.
        pub chain_state_hash: String,
    }

    /// Update to apply to a thread snapshot during event append.
    #[derive(Debug, Clone)]
    pub struct SnapshotUpdate {
        pub thread_id: String,
        pub new_snapshot: ThreadSnapshot,
    }
}

/// File lock for a chain.
pub struct ChainLock {
    #[allow(dead_code)]
    lock_file: File,
    chain_root_id: String,
}

impl ChainLock {
    /// Acquire exclusive lock for a chain.
    ///
    /// Creates a lock file under `state_root/objects/refs/generic/chains/<chain_root_id>/lock`
    pub fn acquire(refs_root: &Path, chain_root_id: &str) -> anyhow::Result<Self> {
        let lock_path = refs_root
            .join("generic/chains")
            .join(chain_root_id)
            .join("lock");

        if let Some(parent) = lock_path.parent() {
            std::fs::create_dir_all(parent).context("failed to create lock directory")?;
        }

        let lock_file = OpenOptions::new()
            .create(true)
            .write(true)
            .open(&lock_path)
            .context("failed to open lock file")?;

        Ok(ChainLock {
            lock_file,
            chain_root_id: chain_root_id.to_string(),
        })
    }

    /// Get the chain root ID this lock protects.
    pub fn chain_root_id(&self) -> &str {
        &self.chain_root_id
    }
}

/// Create a new execution chain with a root thread.
///
/// This is the birth of a chain. A single initial thread_snapshot and chain_state
/// are created and stored in CAS. The signed head ref is written.
///
/// Steps:
/// 1. Create initial thread_snapshot for the root thread
/// 2. Create initial chain_state
/// 3. Store both in CAS
/// 4. Write signed head ref
/// 5. Update head cache
pub fn create_chain(
    cas_root: &Path,
    refs_root: &Path,
    chain_root_id: &str,
    initial_snapshot: ThreadSnapshot,
    signer: &dyn Signer,
    head_cache: &mut HeadCache,
) -> anyhow::Result<CreateResult> {
    // Acquire lock
    let _lock = ChainLock::acquire(refs_root, chain_root_id)?;

    // Validate the initial snapshot
    initial_snapshot.validate()?;
    if initial_snapshot.thread_id != chain_root_id {
        anyhow::bail!(
            "root thread snapshot must have thread_id == chain_root_id (got {})",
            initial_snapshot.thread_id
        );
    }

    // Serialize and store the snapshot in CAS
    let snapshot_value = initial_snapshot.to_value();
    let snapshot_json = lillux::canonical_json(&snapshot_value);
    let snapshot_hash = lillux::sha256_hex(snapshot_json.as_bytes());
    lillux::atomic_write(
        &cas_root.join("objects").join(&snapshot_hash),
        snapshot_json.as_bytes(),
    )
    .context("failed to store initial snapshot in CAS")?;

    // Create initial chain_state
    let mut threads = BTreeMap::new();
    threads.insert(
        chain_root_id.to_string(),
        ChainThreadEntry {
            snapshot_hash: snapshot_hash.clone(),
            last_event_hash: None,
            last_thread_seq: 0,
            status: initial_snapshot.status,
        },
    );

    let chain_state = ChainState {
        schema: 1,
        kind: "chain_state".to_string(),
        chain_root_id: chain_root_id.to_string(),
        prev_chain_state_hash: None,
        last_event_hash: None,
        last_chain_seq: 0,
        updated_at: chrono::Utc::now().to_rfc3339(),
        threads,
    };

    // Validate and store the chain_state in CAS
    chain_state.validate()?;
    let chain_state_value = chain_state.to_value();
    let chain_state_json = lillux::canonical_json(&chain_state_value);
    let chain_state_hash = lillux::sha256_hex(chain_state_json.as_bytes());
    lillux::atomic_write(
        &cas_root.join("objects").join(&chain_state_hash),
        chain_state_json.as_bytes(),
    )
    .context("failed to store initial chain_state in CAS")?;

    // Write signed head ref
    let signed_ref = SignedRef::new(
        format!("chains/{}/head", chain_root_id),
        chain_state_hash.clone(),
        chain_state.updated_at.clone(),
        signer.fingerprint().to_string(),
    );

    let ref_path = refs_root
        .join("generic/chains")
        .join(chain_root_id)
        .join("head");
    refs::write_signed_ref(&ref_path, signed_ref, signer)
        .context("failed to write signed head ref")?;

    // Update head cache
    head_cache.insert(
        chain_root_id.to_string(),
        crate::CachedHead::new(chain_state_hash.clone(), chain_state),
    );

    Ok(CreateResult {
        chain_state_hash,
        snapshot_hash,
        events: vec![],
    })
}

/// Append events to a chain.
///
/// Steps:
/// 1. Acquire lock
/// 2. Read current chain head
/// 3. Allocate sequence numbers
/// 4. Store thread_events in CAS
/// 5. Store updated thread_snapshots (if provided)
/// 6. Create new chain_state
/// 7. Store chain_state in CAS
/// 8. Write signed head ref
/// 9. Update head cache
pub fn append_events(
    cas_root: &Path,
    refs_root: &Path,
    chain_root_id: &str,
    thread_id: &str,
    events: Vec<ThreadEvent>,
    snapshot_updates: Vec<SnapshotUpdate>,
    signer: &dyn Signer,
    head_cache: &mut HeadCache,
) -> anyhow::Result<AppendResult> {
    if events.is_empty() {
        anyhow::bail!("cannot append empty event list");
    }

    // Acquire lock
    let _lock = ChainLock::acquire(refs_root, chain_root_id)?;

    // Read current chain head
    let current_chain_state = read_chain_head(cas_root, refs_root, chain_root_id, head_cache)
        .context("failed to read current chain head")?;

    // Verify thread exists in chain
    if !current_chain_state.threads.contains_key(thread_id) {
        anyhow::bail!("thread {} not found in chain {}", thread_id, chain_root_id);
    }

    let current_thread_entry = &current_chain_state.threads[thread_id];
    let mut first_chain_seq = current_chain_state.last_chain_seq + 1;
    let mut thread_seq = current_thread_entry.last_thread_seq + 1;

    // Validate all events first
    for event in &events {
        event.validate()?;
        if event.chain_root_id != chain_root_id {
            anyhow::bail!(
                "event chain_root_id mismatch: expected {}, got {}",
                chain_root_id,
                event.chain_root_id
            );
        }
        if event.thread_id != thread_id {
            anyhow::bail!(
                "event thread_id mismatch: expected {}, got {}",
                thread_id,
                event.thread_id
            );
        }
    }

    // Store events in CAS
    let mut stored_events = Vec::new();
    let mut last_event_hash: Option<String> = current_chain_state.last_event_hash.clone();

    for mut event in events {
        event.chain_seq = first_chain_seq;
        event.thread_seq = thread_seq;
        event.prev_chain_event_hash = last_event_hash.clone();
        event.prev_thread_event_hash = current_thread_entry.last_event_hash.clone();
        event.validate()?;

        let event_value = event.to_value();
        let event_json = lillux::canonical_json(&event_value);
        let event_hash = lillux::sha256_hex(event_json.as_bytes());
        lillux::atomic_write(
            &cas_root.join("objects").join(&event_hash),
            event_json.as_bytes(),
        )
        .context("failed to store event in CAS")?;

        last_event_hash = Some(event_hash);
        stored_events.push(event);
        first_chain_seq += 1;
        thread_seq += 1;
    }

    let event_count = stored_events.len();
    let first_chain_seq_result = current_chain_state.last_chain_seq + 1;

    // Update thread entry and store new snapshots
    let mut new_threads = current_chain_state.threads.clone();
    let mut root_thread_entry = new_threads[thread_id].clone();
    root_thread_entry.last_event_hash = last_event_hash.clone();
    root_thread_entry.last_thread_seq = thread_seq - 1;

    // Process snapshot updates
    for update in snapshot_updates {
        update.new_snapshot.validate()?;

        let snapshot_value = update.new_snapshot.to_value();
        let snapshot_json = lillux::canonical_json(&snapshot_value);
        let snapshot_hash = lillux::sha256_hex(snapshot_json.as_bytes());

        lillux::atomic_write(
            &cas_root.join("objects").join(&snapshot_hash),
            snapshot_json.as_bytes(),
        )
        .context("failed to store snapshot in CAS")?;

        let thread_entry = new_threads.get_mut(&update.thread_id).ok_or_else(|| {
            anyhow!(
                "attempted to update snapshot for unknown thread {}",
                update.thread_id
            )
        })?;

        thread_entry.snapshot_hash = snapshot_hash;
        thread_entry.status = update.new_snapshot.status;
    }

    // Always update the appending thread's entry
    new_threads.insert(thread_id.to_string(), root_thread_entry);

    // Create new chain_state
    let new_chain_state = ChainState {
        schema: 1,
        kind: "chain_state".to_string(),
        chain_root_id: chain_root_id.to_string(),
        prev_chain_state_hash: Some(
            lillux::sha256_hex(lillux::canonical_json(&current_chain_state.to_value()).as_bytes()),
        ),
        last_event_hash,
        last_chain_seq: thread_seq - 1,
        updated_at: chrono::Utc::now().to_rfc3339(),
        threads: new_threads,
    };

    // Store chain_state in CAS
    new_chain_state.validate()?;
    let chain_state_value = new_chain_state.to_value();
    let chain_state_json = lillux::canonical_json(&chain_state_value);
    let chain_state_hash = lillux::sha256_hex(chain_state_json.as_bytes());
    lillux::atomic_write(
        &cas_root.join("objects").join(&chain_state_hash),
        chain_state_json.as_bytes(),
    )
    .context("failed to store new chain_state in CAS")?;

    // Write signed head ref
    let signed_ref = SignedRef::new(
        format!("chains/{}/head", chain_root_id),
        chain_state_hash.clone(),
        new_chain_state.updated_at.clone(),
        signer.fingerprint().to_string(),
    );

    let ref_path = refs_root
        .join("generic/chains")
        .join(chain_root_id)
        .join("head");
    refs::write_signed_ref(&ref_path, signed_ref, signer)
        .context("failed to write signed head ref")?;

    // Update head cache
    head_cache.insert(
        chain_root_id.to_string(),
        crate::CachedHead::new(chain_state_hash.clone(), new_chain_state),
    );

    Ok(AppendResult {
        chain_state_hash,
        first_chain_seq: first_chain_seq_result,
        event_count,
        events: stored_events,
    })
}

/// Read the current chain head.
///
/// First tries the in-memory head cache. If not found, reads and verifies
/// the signed ref from the refs directory.
pub fn read_chain_head(
    cas_root: &Path,
    refs_root: &Path,
    chain_root_id: &str,
    head_cache: &HeadCache,
) -> anyhow::Result<ChainState> {
    // Try cache first
    if let Some(cached_head) = head_cache.get(chain_root_id) {
        return Ok(cached_head.chain_state.clone());
    }

    // Cache miss: read signed ref and load from CAS
    let ref_path = refs_root
        .join("generic/chains")
        .join(chain_root_id)
        .join("head");

    if !ref_path.exists() {
        anyhow::bail!("signed ref not found for chain {}", chain_root_id);
    }

    let signed_ref = refs::read_signed_ref(&ref_path)
        .context("failed to read signed ref")?;

    // Load chain_state from CAS
    let cas_path = cas_root
        .join("objects")
        .join(&signed_ref.target_hash);
    let chain_state_json = std::fs::read_to_string(&cas_path)
        .context("failed to read chain_state from CAS")?;

    let chain_state: ChainState =
        serde_json::from_str(&chain_state_json).context("failed to parse chain_state")?;

    chain_state.validate()?;

    Ok(chain_state)
}

/// Read a thread's snapshot from a chain.
///
/// Returns None if the thread doesn't exist in the chain.
pub fn read_thread_snapshot(
    cas_root: &Path,
    refs_root: &Path,
    chain_root_id: &str,
    thread_id: &str,
    head_cache: &HeadCache,
) -> anyhow::Result<ReadSnapshotResult> {
    let chain_state =
        read_chain_head(cas_root, refs_root, chain_root_id, head_cache)?;

    let chain_state_hash =
        lillux::sha256_hex(lillux::canonical_json(&chain_state.to_value()).as_bytes());

    // Check if thread exists in chain
    let thread_entry = match chain_state.threads.get(thread_id) {
        Some(entry) => entry,
        None => {
            return Ok(ReadSnapshotResult {
                snapshot: None,
                chain_state_hash,
            });
        }
    };

    // Load snapshot from CAS
    let cas_path = cas_root
        .join("objects")
        .join(&thread_entry.snapshot_hash);
    let snapshot_json = std::fs::read_to_string(&cas_path)
        .context("failed to read snapshot from CAS")?;

    let snapshot: ThreadSnapshot =
        serde_json::from_str(&snapshot_json).context("failed to parse snapshot")?;

    snapshot.validate()?;

    Ok(ReadSnapshotResult {
        snapshot: Some(snapshot),
        chain_state_hash,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signer::TestSigner;

    fn make_test_snapshot(thread_id: &str) -> ThreadSnapshot {
        use crate::objects::thread_snapshot::ThreadSnapshotBuilder;
        
        ThreadSnapshotBuilder::new(
            thread_id,
            thread_id,
            "directive",
            "system/test",
            "directive-runtime",
        )
        .last_event_hash(Some("00".repeat(32)))
        .build()
    }

    fn make_test_event(chain_root_id: &str, thread_id: &str) -> ThreadEvent {
        use crate::objects::thread_event::NewEvent;
        
        NewEvent::new(chain_root_id, thread_id, "test_event")
            .payload(serde_json::json!({}))
            .build()
    }

    #[test]
    fn create_chain_succeeds() {
        let tempdir = tempfile::tempdir().unwrap();
        let cas_root = tempdir.path().join("state");
        let refs_root = tempdir.path().join("refs");
        let signer = TestSigner::default();
        let mut head_cache = HeadCache::new();

        let snapshot = make_test_snapshot("T-root");
        let result = create_chain(
            &cas_root,
            &refs_root,
            "T-root",
            snapshot,
            &signer,
            &mut head_cache,
        );

        assert!(result.is_ok());
        let result = result.unwrap();
        assert!(!result.chain_state_hash.is_empty());
        assert!(!result.snapshot_hash.is_empty());
    }

    #[test]
    fn create_chain_writes_cas_objects() {
        let tempdir = tempfile::tempdir().unwrap();
        let cas_root = tempdir.path().join("state");
        let refs_root = tempdir.path().join("refs");
        let signer = TestSigner::default();
        let mut head_cache = HeadCache::new();

        let snapshot = make_test_snapshot("T-root");
        let result = create_chain(
            &cas_root,
            &refs_root,
            "T-root",
            snapshot,
            &signer,
            &mut head_cache,
        )
        .unwrap();

        // Verify snapshot object exists in CAS
        let snapshot_path = cas_root.join("objects").join(&result.snapshot_hash);
        assert!(snapshot_path.exists());

        // Verify chain_state object exists in CAS
        let chain_state_path = cas_root.join("objects").join(&result.chain_state_hash);
        assert!(chain_state_path.exists());
    }

    #[test]
    fn create_chain_writes_signed_ref() {
        let tempdir = tempfile::tempdir().unwrap();
        let cas_root = tempdir.path().join("state");
        let refs_root = tempdir.path().join("refs");
        let signer = TestSigner::default();
        let mut head_cache = HeadCache::new();

        let snapshot = make_test_snapshot("T-root");
        create_chain(
            &cas_root,
            &refs_root,
            "T-root",
            snapshot,
            &signer,
            &mut head_cache,
        )
        .unwrap();

        let ref_path = refs_root.join("generic/chains/T-root/head");
        assert!(ref_path.exists());

        let signed_ref = refs::read_signed_ref(&ref_path).unwrap();
        assert_eq!(signed_ref.ref_path, "chains/T-root/head");
    }

    #[test]
    fn create_chain_updates_head_cache() {
        let tempdir = tempfile::tempdir().unwrap();
        let cas_root = tempdir.path().join("state");
        let refs_root = tempdir.path().join("refs");
        let signer = TestSigner::default();
        let mut head_cache = HeadCache::new();

        let snapshot = make_test_snapshot("T-root");
        let result = create_chain(
            &cas_root,
            &refs_root,
            "T-root",
            snapshot,
            &signer,
            &mut head_cache,
        )
        .unwrap();

        assert!(head_cache.contains("T-root"));
        assert_eq!(
            head_cache.get_hash("T-root"),
            Some(result.chain_state_hash.as_str())
        );
    }

    #[test]
    fn append_events_succeeds() {
        let tempdir = tempfile::tempdir().unwrap();
        let cas_root = tempdir.path().join("state");
        let refs_root = tempdir.path().join("refs");
        let signer = TestSigner::default();
        let mut head_cache = HeadCache::new();

        // Create chain first
        let snapshot = make_test_snapshot("T-root");
        create_chain(
            &cas_root,
            &refs_root,
            "T-root",
            snapshot,
            &signer,
            &mut head_cache,
        )
        .unwrap();

        // Append events
        let event = make_test_event("T-root", "T-root");
        let result = append_events(
            &cas_root,
            &refs_root,
            "T-root",
            "T-root",
            vec![event],
            vec![],
            &signer,
            &mut head_cache,
        );

        assert!(result.is_ok());
        let result = result.unwrap();
        assert_eq!(result.event_count, 1);
        assert_eq!(result.first_chain_seq, 1);
    }

    #[test]
    fn append_events_stores_in_cas() {
        let tempdir = tempfile::tempdir().unwrap();
        let cas_root = tempdir.path().join("state");
        let refs_root = tempdir.path().join("refs");
        let signer = TestSigner::default();
        let mut head_cache = HeadCache::new();

        // Create chain
        let snapshot = make_test_snapshot("T-root");
        create_chain(
            &cas_root,
            &refs_root,
            "T-root",
            snapshot,
            &signer,
            &mut head_cache,
        )
        .unwrap();

        // Append events
        let event = make_test_event("T-root", "T-root");
        let result = append_events(
            &cas_root,
            &refs_root,
            "T-root",
            "T-root",
            vec![event],
            vec![],
            &signer,
            &mut head_cache,
        )
        .unwrap();

        // Verify new chain_state exists in CAS
        let chain_state_path = cas_root.join("objects").join(&result.chain_state_hash);
        assert!(chain_state_path.exists());
    }

    #[test]
    fn read_chain_head_from_cache() {
        let tempdir = tempfile::tempdir().unwrap();
        let cas_root = tempdir.path().join("state");
        let refs_root = tempdir.path().join("refs");
        let signer = TestSigner::default();
        let mut head_cache = HeadCache::new();

        // Create chain
        let snapshot = make_test_snapshot("T-root");
        create_chain(
            &cas_root,
            &refs_root,
            "T-root",
            snapshot,
            &signer,
            &mut head_cache,
        )
        .unwrap();

        // Read head (should hit cache)
        let head = read_chain_head(&cas_root, &refs_root, "T-root", &head_cache).unwrap();
        assert_eq!(head.chain_root_id, "T-root");
        assert_eq!(head.last_chain_seq, 0);
    }

    #[test]
    fn read_thread_snapshot_succeeds() {
        let tempdir = tempfile::tempdir().unwrap();
        let cas_root = tempdir.path().join("state");
        let refs_root = tempdir.path().join("refs");
        let signer = TestSigner::default();
        let mut head_cache = HeadCache::new();

        // Create chain
        let snapshot = make_test_snapshot("T-root");
        create_chain(
            &cas_root,
            &refs_root,
            "T-root",
            snapshot,
            &signer,
            &mut head_cache,
        )
        .unwrap();

        // Read snapshot
        let result = read_thread_snapshot(
            &cas_root,
            &refs_root,
            "T-root",
            "T-root",
            &head_cache,
        )
        .unwrap();

        assert!(result.snapshot.is_some());
        let snapshot = result.snapshot.unwrap();
        assert_eq!(snapshot.thread_id, "T-root");
    }

    #[test]
    fn read_thread_snapshot_returns_none_for_missing_thread() {
        let tempdir = tempfile::tempdir().unwrap();
        let cas_root = tempdir.path().join("state");
        let refs_root = tempdir.path().join("refs");
        let signer = TestSigner::default();
        let mut head_cache = HeadCache::new();

        // Create chain
        let snapshot = make_test_snapshot("T-root");
        create_chain(
            &cas_root,
            &refs_root,
            "T-root",
            snapshot,
            &signer,
            &mut head_cache,
        )
        .unwrap();

        // Try to read non-existent thread
        let result = read_thread_snapshot(
            &cas_root,
            &refs_root,
            "T-root",
            "T-missing",
            &head_cache,
        )
        .unwrap();

        assert!(result.snapshot.is_none());
    }

    #[test]
    fn append_events_rejects_empty() {
        let tempdir = tempfile::tempdir().unwrap();
        let cas_root = tempdir.path().join("state");
        let refs_root = tempdir.path().join("refs");
        let signer = TestSigner::default();
        let mut head_cache = HeadCache::new();

        let snapshot = make_test_snapshot("T-root");
        create_chain(
            &cas_root,
            &refs_root,
            "T-root",
            snapshot,
            &signer,
            &mut head_cache,
        )
        .unwrap();

        let result = append_events(
            &cas_root,
            &refs_root,
            "T-root",
            "T-root",
            vec![],
            vec![],
            &signer,
            &mut head_cache,
        );

        assert!(result.is_err());
    }

    #[test]
    fn append_events_rejects_wrong_chain_id() {
        let tempdir = tempfile::tempdir().unwrap();
        let cas_root = tempdir.path().join("state");
        let refs_root = tempdir.path().join("refs");
        let signer = TestSigner::default();
        let mut head_cache = HeadCache::new();

        let snapshot = make_test_snapshot("T-root");
        create_chain(
            &cas_root,
            &refs_root,
            "T-root",
            snapshot,
            &signer,
            &mut head_cache,
        )
        .unwrap();

        let event = make_test_event("T-wrong", "T-root");
        let result = append_events(
            &cas_root,
            &refs_root,
            "T-root",
            "T-root",
            vec![event],
            vec![],
            &signer,
            &mut head_cache,
        );

        assert!(result.is_err());
    }

    #[test]
    fn append_events_rejects_wrong_thread_id() {
        let tempdir = tempfile::tempdir().unwrap();
        let cas_root = tempdir.path().join("state");
        let refs_root = tempdir.path().join("refs");
        let signer = TestSigner::default();
        let mut head_cache = HeadCache::new();

        let snapshot = make_test_snapshot("T-root");
        create_chain(
            &cas_root,
            &refs_root,
            "T-root",
            snapshot,
            &signer,
            &mut head_cache,
        )
        .unwrap();

        let event = make_test_event("T-root", "T-wrong");
        let result = append_events(
            &cas_root,
            &refs_root,
            "T-root",
            "T-root",
            vec![event],
            vec![],
            &signer,
            &mut head_cache,
        );

        assert!(result.is_err());
    }
}
