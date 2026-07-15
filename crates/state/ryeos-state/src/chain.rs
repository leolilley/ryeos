//! Chain operations — create, append, read, lock.
//!
//! Every execution chain is a hash-linked sequence of immutable CAS objects.
//! All writes are protected by file locks and validated against the signed head ref.

use std::collections::{BTreeMap, HashSet};
use std::fs::{File, OpenOptions};
use std::path::Path;

use anyhow::{anyhow, Context};

use crate::objects::{ChainState, ChainThreadEntry, ThreadEvent, ThreadSnapshot};
use crate::signer::Signer;
use crate::{refs, CachedHead, HeadCache, SignedRef};

pub use self::types::*;

mod types {
    use crate::objects::{ThreadEvent, ThreadSnapshot};

    /// Durability state of the authoritative chain-head namespace update.
    ///
    /// `Uncertain` means the new head is already visible and therefore
    /// committed, but the post-rename durability barrier failed. Callers must
    /// not roll the operation back; they may retain recovery markers and
    /// surface the warning for a later reconciliation pass.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum HeadDurability {
        Durable,
        Uncertain(String),
    }

    impl HeadDurability {
        pub fn is_durable(&self) -> bool {
            matches!(self, Self::Durable)
        }
    }

    /// Result of creating a new execution chain.
    #[derive(Debug, Clone)]
    pub struct CreateResult {
        /// CAS hash of the initial chain state.
        pub chain_state_hash: String,
        /// CAS hash of the initial thread snapshot.
        pub snapshot_hash: String,
        /// The events that were persisted.
        pub events: Vec<ThreadEvent>,
        /// Durability state of the committed head ref.
        pub head_durability: HeadDurability,
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
        /// Durability state of the committed head ref.
        pub head_durability: HeadDurability,
    }

    /// Result of adding a thread and its initial events in one chain update.
    #[derive(Debug, Clone)]
    pub struct AddThreadWithEventsResult {
        /// CAS hash of the new chain state.
        pub chain_state_hash: String,
        /// CAS hash of the new thread snapshot.
        pub snapshot_hash: String,
        /// Sequence number of the first appended event.
        pub first_chain_seq: u64,
        /// Number of events appended.
        pub event_count: usize,
        /// The events that were persisted.
        pub events: Vec<ThreadEvent>,
        /// Durability state of the committed head ref.
        pub head_durability: HeadDurability,
    }

    /// Result of atomically committing a running-source continuation handoff.
    #[derive(Debug, Clone)]
    pub struct CommitContinuationResult {
        /// CAS hash of the new chain state.
        pub chain_state_hash: String,
        /// The exact finalized source snapshot stored in CAS.
        pub source_snapshot: ThreadSnapshot,
        /// The exact finalized successor snapshot stored in CAS.
        pub successor_snapshot: ThreadSnapshot,
        /// CAS hash of the newly-created successor snapshot.
        pub successor_snapshot_hash: String,
        /// The source and successor events in committed chain order.
        pub events: Vec<ThreadEvent>,
        /// Durability state of the committed head ref.
        pub head_durability: HeadDurability,
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

fn classify_head_write(
    result: lillux::AtomicMutationResult<()>,
    operation: &'static str,
) -> anyhow::Result<HeadDurability> {
    match result {
        Ok(()) => Ok(HeadDurability::Durable),
        Err(lillux::AtomicMutationError::BeforeCommit(error)) => Err(error.context(operation)),
        Err(lillux::AtomicMutationError::DurabilityUncertain(error)) => {
            let warning = format!("{operation}: {error:#}");
            tracing::warn!(error = %warning, "chain head committed with uncertain durability");
            Ok(HeadDurability::Uncertain(warning))
        }
    }
}

/// Parameters for [`append_events`].
pub struct AppendEventsInput<'a> {
    pub cas_root: &'a Path,
    pub refs_root: &'a Path,
    pub chain_root_id: &'a str,
    pub thread_id: &'a str,
    pub events: Vec<ThreadEvent>,
    pub snapshot_updates: Vec<SnapshotUpdate>,
    pub signer: &'a dyn Signer,
}

/// Parameters for [`commit_continuation`].
pub struct CommitContinuationInput<'a> {
    pub cas_root: &'a Path,
    pub refs_root: &'a Path,
    pub chain_root_id: &'a str,
    pub source_thread_id: &'a str,
    pub expected_source_snapshot_hash: &'a str,
    pub source_snapshot: ThreadSnapshot,
    pub successor_snapshot: ThreadSnapshot,
    pub source_event: ThreadEvent,
    pub successor_event: ThreadEvent,
    pub signer: &'a dyn Signer,
}

/// File lock for a chain.
pub struct ChainLock {
    /// Held only for RAII; Drop releases the file lock via closed fd.
    _lock_file: File,
    chain_root_id: String,
}

impl ChainLock {
    /// Acquire exclusive lock for a chain using flock.
    ///
    /// Creates a lock file under `runtime_state_dir/objects/refs/generic/chains/<chain_root_id>/lock`
    /// and acquires an exclusive file lock via flock(2).
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
            .truncate(true)
            .write(true)
            .open(&lock_path)
            .context("failed to open lock file")?;

        // Acquire exclusive flock on Unix
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            let ret = unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX) };
            if ret != 0 {
                anyhow::bail!("flock failed: {}", std::io::Error::last_os_error());
            }
        }

        Ok(ChainLock {
            _lock_file: lock_file,
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
#[tracing::instrument(
    name = "state:chain_create",
    skip(cas_root, refs_root, initial_snapshot, signer, head_cache),
    fields(chain_root_id = %chain_root_id)
)]
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
    let snapshot_path = lillux::shard_path(cas_root, "objects", &snapshot_hash, ".json");
    lillux::atomic_write(&snapshot_path, snapshot_json.as_bytes())
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
        updated_at: lillux::time::iso8601_now(),
        threads,
    };

    // Validate and store the chain_state in CAS
    chain_state.validate()?;
    let chain_state_value = chain_state.to_value();
    let chain_state_json = lillux::canonical_json(&chain_state_value);
    let chain_state_hash = lillux::sha256_hex(chain_state_json.as_bytes());
    let chain_state_path = lillux::shard_path(cas_root, "objects", &chain_state_hash, ".json");
    lillux::atomic_write(&chain_state_path, chain_state_json.as_bytes())
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
    let head_durability = classify_head_write(
        refs::write_signed_ref(&ref_path, signed_ref, signer),
        "failed to write signed head ref",
    )?;

    tracing::trace!(chain_root_id = %chain_root_id, head_hash = %chain_state_hash, "chain created and head ref written");

    // Update head cache
    head_cache.insert(
        chain_root_id.to_string(),
        crate::CachedHead::new(chain_state_hash.clone(), chain_state),
    );

    Ok(CreateResult {
        chain_state_hash,
        snapshot_hash,
        events: vec![],
        head_durability,
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
#[tracing::instrument(
    name = "state:chain_append",
    skip(input, head_cache),
    fields(
        chain_root_id = %input.chain_root_id,
        thread_id = %input.thread_id,
        event_count = input.events.len(),
    )
)]
pub fn append_events(
    input: AppendEventsInput<'_>,
    head_cache: &mut HeadCache,
) -> anyhow::Result<AppendResult> {
    let AppendEventsInput {
        cas_root,
        refs_root,
        chain_root_id,
        thread_id,
        events,
        snapshot_updates,
        signer,
    } = input;

    if events.is_empty() {
        anyhow::bail!("cannot append empty event list");
    }

    // Acquire lock
    let _lock = ChainLock::acquire(refs_root, chain_root_id)?;

    // Read current chain head
    let trust_store = crate::signer::trust_store_for_signer(signer);
    let current_chain_state = read_chain_head(
        cas_root,
        refs_root,
        chain_root_id,
        &trust_store,
        head_cache,
    )
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
    let mut last_thread_event_hash: Option<String> = current_thread_entry.last_event_hash.clone();

    let mut pending_writes: Vec<(std::path::PathBuf, Vec<u8>)> = Vec::new();
    for mut event in events {
        event.chain_seq = first_chain_seq;
        event.thread_seq = thread_seq;
        event.prev_chain_event_hash = last_event_hash.clone();
        event.prev_thread_event_hash = last_thread_event_hash.clone();
        event.validate()?;

        let event_value = event.to_value();
        let event_json = lillux::canonical_json(&event_value);
        let event_hash = lillux::sha256_hex(event_json.as_bytes());
        let event_path = lillux::shard_path(cas_root, "objects", &event_hash, ".json");
        pending_writes.push((event_path, event_json.into_bytes()));

        last_event_hash = Some(event_hash.clone());
        last_thread_event_hash = Some(event_hash);
        stored_events.push(event);
        first_chain_seq += 1;
        thread_seq += 1;
    }
    // One durability barrier for the whole batch instead of one fsync
    // per event: nothing references these objects until the chain state
    // below advances, so partial batches after a crash are unreachable
    // garbage, not corruption.
    lillux::atomic_write_batch(&pending_writes).context("failed to store events in CAS")?;

    let event_count = stored_events.len();
    let first_chain_seq_result = current_chain_state.last_chain_seq + 1;

    // Update thread entry and store new snapshots
    let mut new_threads = current_chain_state.threads.clone();

    // Process snapshot updates
    for update in snapshot_updates {
        update.new_snapshot.validate()?;

        let snapshot_value = update.new_snapshot.to_value();
        let snapshot_json = lillux::canonical_json(&snapshot_value);
        let snapshot_hash = lillux::sha256_hex(snapshot_json.as_bytes());

        let snapshot_path = lillux::shard_path(cas_root, "objects", &snapshot_hash, ".json");
        lillux::atomic_write(&snapshot_path, snapshot_json.as_bytes())
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

    // Always update the appending thread's entry (use the potentially-updated entry)
    if let Some(entry) = new_threads.get_mut(thread_id) {
        entry.last_event_hash = last_event_hash.clone();
        entry.last_thread_seq = thread_seq - 1;
    }

    // Create new chain_state
    let new_chain_state = ChainState {
        schema: 1,
        kind: "chain_state".to_string(),
        chain_root_id: chain_root_id.to_string(),
        prev_chain_state_hash: Some(lillux::sha256_hex(
            lillux::canonical_json(&current_chain_state.to_value()).as_bytes(),
        )),
        last_event_hash,
        last_chain_seq: first_chain_seq - 1,
        updated_at: lillux::time::iso8601_now(),
        threads: new_threads,
    };

    // Store chain_state in CAS
    new_chain_state.validate()?;
    let chain_state_value = new_chain_state.to_value();
    let chain_state_json = lillux::canonical_json(&chain_state_value);
    let chain_state_hash = lillux::sha256_hex(chain_state_json.as_bytes());
    let chain_state_path = lillux::shard_path(cas_root, "objects", &chain_state_hash, ".json");
    lillux::atomic_write(&chain_state_path, chain_state_json.as_bytes())
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
    let head_durability = classify_head_write(
        refs::write_signed_ref(&ref_path, signed_ref, signer),
        "failed to write signed head ref",
    )?;

    tracing::trace!(chain_root_id = %chain_root_id, new_chain_seq = new_chain_state.last_chain_seq, events_appended = stored_events.len(), "events appended to chain");

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
        head_durability,
    })
}

/// Add a new thread to an existing chain.
///
/// Stores the thread's initial snapshot in CAS and updates the chain_state
/// to include the new thread entry. Used when spawning child threads.
pub fn add_thread_to_chain(
    cas_root: &Path,
    refs_root: &Path,
    chain_root_id: &str,
    new_snapshot: ThreadSnapshot,
    signer: &dyn Signer,
    head_cache: &mut HeadCache,
) -> anyhow::Result<CreateResult> {
    // Validate
    new_snapshot.validate()?;
    if new_snapshot.chain_root_id != chain_root_id {
        anyhow::bail!(
            "snapshot chain_root_id mismatch: expected {}, got {}",
            chain_root_id,
            new_snapshot.chain_root_id
        );
    }
    if new_snapshot.thread_id == chain_root_id {
        anyhow::bail!("use create_chain() for root threads, not add_thread_to_chain()");
    }

    // Acquire lock
    let _lock = ChainLock::acquire(refs_root, chain_root_id)?;

    // Read current chain head
    let trust_store = crate::signer::trust_store_for_signer(signer);
    let current_chain_state = read_chain_head(
        cas_root,
        refs_root,
        chain_root_id,
        &trust_store,
        head_cache,
    )
    .context("failed to read current chain head")?;

    // Verify thread doesn't already exist
    if current_chain_state
        .threads
        .contains_key(&new_snapshot.thread_id)
    {
        anyhow::bail!(
            "thread {} already exists in chain {}",
            new_snapshot.thread_id,
            chain_root_id
        );
    }

    // Store snapshot in CAS
    let snapshot_value = new_snapshot.to_value();
    let snapshot_json = lillux::canonical_json(&snapshot_value);
    let snapshot_hash = lillux::sha256_hex(snapshot_json.as_bytes());
    let snapshot_path = lillux::shard_path(cas_root, "objects", &snapshot_hash, ".json");
    lillux::atomic_write(&snapshot_path, snapshot_json.as_bytes())
        .context("failed to store child snapshot in CAS")?;

    // Build new chain_state with the additional thread
    let mut new_threads = current_chain_state.threads.clone();
    new_threads.insert(
        new_snapshot.thread_id.clone(),
        ChainThreadEntry {
            snapshot_hash: snapshot_hash.clone(),
            last_event_hash: None,
            last_thread_seq: 0,
            status: new_snapshot.status,
        },
    );

    let prev_hash =
        lillux::sha256_hex(lillux::canonical_json(&current_chain_state.to_value()).as_bytes());

    let new_chain_state = ChainState {
        schema: 1,
        kind: "chain_state".to_string(),
        chain_root_id: chain_root_id.to_string(),
        prev_chain_state_hash: Some(prev_hash),
        last_event_hash: current_chain_state.last_event_hash.clone(),
        last_chain_seq: current_chain_state.last_chain_seq,
        updated_at: lillux::time::iso8601_now(),
        threads: new_threads,
    };

    // Store new chain_state in CAS
    new_chain_state.validate()?;
    let chain_state_value = new_chain_state.to_value();
    let chain_state_json = lillux::canonical_json(&chain_state_value);
    let chain_state_hash = lillux::sha256_hex(chain_state_json.as_bytes());
    let chain_state_path = lillux::shard_path(cas_root, "objects", &chain_state_hash, ".json");
    lillux::atomic_write(&chain_state_path, chain_state_json.as_bytes())
        .context("failed to store updated chain_state in CAS")?;

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
    let head_durability = classify_head_write(
        refs::write_signed_ref(&ref_path, signed_ref, signer),
        "failed to write signed head ref",
    )?;

    // Update head cache
    head_cache.insert(
        chain_root_id.to_string(),
        crate::CachedHead::new(chain_state_hash.clone(), new_chain_state),
    );

    tracing::trace!(chain_root_id = %chain_root_id, thread_id = %new_snapshot.thread_id, snapshot_hash = %snapshot_hash, "thread added to chain");
    Ok(CreateResult {
        chain_state_hash,
        snapshot_hash,
        events: vec![],
        head_durability,
    })
}

pub fn add_thread_to_chain_with_events(
    cas_root: &Path,
    refs_root: &Path,
    chain_root_id: &str,
    new_snapshot: ThreadSnapshot,
    events: Vec<ThreadEvent>,
    signer: &dyn Signer,
    head_cache: &mut HeadCache,
) -> anyhow::Result<AddThreadWithEventsResult> {
    new_snapshot.validate()?;
    if new_snapshot.chain_root_id != chain_root_id {
        anyhow::bail!(
            "snapshot chain_root_id mismatch: expected {}, got {}",
            chain_root_id,
            new_snapshot.chain_root_id
        );
    }
    if new_snapshot.thread_id == chain_root_id {
        anyhow::bail!("use create_chain() for root threads, not add_thread_to_chain_with_events()");
    }
    if events.is_empty() {
        anyhow::bail!("cannot add thread with empty event list");
    }

    for event in &events {
        event.validate()?;
        if event.chain_root_id != chain_root_id {
            anyhow::bail!(
                "event chain_root_id mismatch: expected {}, got {}",
                chain_root_id,
                event.chain_root_id
            );
        }
        if event.thread_id != new_snapshot.thread_id {
            anyhow::bail!(
                "event thread_id mismatch: expected {}, got {}",
                new_snapshot.thread_id,
                event.thread_id
            );
        }
    }

    let _lock = ChainLock::acquire(refs_root, chain_root_id)?;
    let trust_store = crate::signer::trust_store_for_signer(signer);
    let current_chain_state = read_chain_head(
        cas_root,
        refs_root,
        chain_root_id,
        &trust_store,
        head_cache,
    )
    .context("failed to read current chain head")?;

    if current_chain_state
        .threads
        .contains_key(&new_snapshot.thread_id)
    {
        anyhow::bail!(
            "thread {} already exists in chain {}",
            new_snapshot.thread_id,
            chain_root_id
        );
    }

    let snapshot_value = new_snapshot.to_value();
    let snapshot_json = lillux::canonical_json(&snapshot_value);
    let snapshot_hash = lillux::sha256_hex(snapshot_json.as_bytes());
    let snapshot_path = lillux::shard_path(cas_root, "objects", &snapshot_hash, ".json");

    let mut chain_seq = current_chain_state.last_chain_seq + 1;
    let first_chain_seq = chain_seq;
    let mut thread_seq = 1;
    let mut last_event_hash = current_chain_state.last_event_hash.clone();
    let mut last_thread_event_hash: Option<String> = None;
    let mut stored_events = Vec::with_capacity(events.len());
    let mut pending_writes: Vec<(std::path::PathBuf, Vec<u8>)> =
        vec![(snapshot_path, snapshot_json.into_bytes())];

    for mut event in events {
        event.chain_seq = chain_seq;
        event.thread_seq = thread_seq;
        event.prev_chain_event_hash = last_event_hash.clone();
        event.prev_thread_event_hash = last_thread_event_hash.clone();
        event.validate()?;

        let event_value = event.to_value();
        let event_json = lillux::canonical_json(&event_value);
        let event_hash = lillux::sha256_hex(event_json.as_bytes());
        let event_path = lillux::shard_path(cas_root, "objects", &event_hash, ".json");
        pending_writes.push((event_path, event_json.into_bytes()));

        last_event_hash = Some(event_hash.clone());
        last_thread_event_hash = Some(event_hash);
        stored_events.push(event);
        chain_seq += 1;
        thread_seq += 1;
    }

    let event_count = stored_events.len();
    let last_thread_seq = thread_seq - 1;

    let mut new_threads = current_chain_state.threads.clone();
    new_threads.insert(
        new_snapshot.thread_id.clone(),
        ChainThreadEntry {
            snapshot_hash: snapshot_hash.clone(),
            last_event_hash: last_thread_event_hash,
            last_thread_seq,
            status: new_snapshot.status,
        },
    );

    let prev_hash =
        lillux::sha256_hex(lillux::canonical_json(&current_chain_state.to_value()).as_bytes());
    let new_chain_state = ChainState {
        schema: 1,
        kind: "chain_state".to_string(),
        chain_root_id: chain_root_id.to_string(),
        prev_chain_state_hash: Some(prev_hash),
        last_event_hash,
        last_chain_seq: chain_seq - 1,
        updated_at: lillux::time::iso8601_now(),
        threads: new_threads,
    };

    new_chain_state.validate()?;
    let chain_state_value = new_chain_state.to_value();
    let chain_state_json = lillux::canonical_json(&chain_state_value);
    let chain_state_hash = lillux::sha256_hex(chain_state_json.as_bytes());
    let chain_state_path = lillux::shard_path(cas_root, "objects", &chain_state_hash, ".json");
    pending_writes.push((chain_state_path, chain_state_json.into_bytes()));
    lillux::atomic_write_batch(&pending_writes)
        .context("failed to store branch thread objects in CAS")?;

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
    let head_durability = classify_head_write(
        refs::write_signed_ref(&ref_path, signed_ref, signer),
        "failed to write signed head ref",
    )?;

    head_cache.insert(
        chain_root_id.to_string(),
        crate::CachedHead::new(chain_state_hash.clone(), new_chain_state),
    );

    Ok(AddThreadWithEventsResult {
        chain_state_hash,
        snapshot_hash,
        first_chain_seq,
        event_count,
        events: stored_events,
        head_durability,
    })
}

fn continuation_successor_from_chain(
    cas_root: &Path,
    chain_root_id: &str,
    source_thread_id: &str,
    source_entry: &ChainThreadEntry,
) -> anyhow::Result<Option<String>> {
    let mut next_hash = source_entry.last_event_hash.clone();
    let mut expected_thread_seq = source_entry.last_thread_seq;
    let mut visited = HashSet::new();

    while let Some(event_hash) = next_hash {
        if expected_thread_seq == 0 {
            anyhow::bail!(
                "thread {} event chain exceeds recorded last_thread_seq",
                source_thread_id
            );
        }
        if !visited.insert(event_hash.clone()) {
            anyhow::bail!("cycle in thread event chain for {source_thread_id}");
        }
        let event_path = lillux::shard_path(cas_root, "objects", &event_hash, ".json");
        let event_json = std::fs::read_to_string(&event_path).with_context(|| {
            format!("failed to read event {event_hash} for thread {source_thread_id}")
        })?;
        let event: ThreadEvent = serde_json::from_str(&event_json).with_context(|| {
            format!("failed to decode event {event_hash} for thread {source_thread_id}")
        })?;
        event.validate()?;
        let actual_hash = lillux::sha256_hex(lillux::canonical_json(&event.to_value()).as_bytes());
        if actual_hash != event_hash {
            anyhow::bail!(
                "event hash mismatch for thread {}: expected {}, got {}",
                source_thread_id,
                event_hash,
                actual_hash
            );
        }
        if event.chain_root_id != chain_root_id || event.thread_id != source_thread_id {
            anyhow::bail!(
                "thread event chain identity mismatch for source {}",
                source_thread_id
            );
        }
        if event.thread_seq != expected_thread_seq {
            anyhow::bail!(
                "thread event sequence mismatch for {}: expected {}, got {}",
                source_thread_id,
                expected_thread_seq,
                event.thread_seq
            );
        }
        if event.event_type == crate::event_types::THREAD_CONTINUED {
            let successor = event
                .payload
                .get("successor_thread_id")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    anyhow!(
                        "thread_continued event for {} is missing successor_thread_id",
                        source_thread_id
                    )
                })?;
            return Ok(Some(successor.to_string()));
        }
        next_hash = event.prev_thread_event_hash.clone();
        expected_thread_seq -= 1;
    }
    if expected_thread_seq != 0 {
        anyhow::bail!(
            "thread event chain for {} ended before recorded sequence {}",
            source_thread_id,
            expected_thread_seq
        );
    }
    Ok(None)
}

/// Atomically create a continuation successor and settle its running source.
///
/// Both snapshots and both events become reachable through one signed chain-head
/// update. A crash before the head write can leave only unreachable CAS objects;
/// it can never expose a successor without the source's `thread_continued` event
/// or expose a continued source without the successor's creation event.
pub fn commit_continuation(
    input: CommitContinuationInput<'_>,
    head_cache: &mut HeadCache,
) -> anyhow::Result<CommitContinuationResult> {
    let CommitContinuationInput {
        cas_root,
        refs_root,
        chain_root_id,
        source_thread_id,
        expected_source_snapshot_hash,
        mut source_snapshot,
        mut successor_snapshot,
        source_event,
        successor_event,
        signer,
    } = input;

    source_snapshot.validate()?;
    successor_snapshot.validate()?;
    for snapshot in [&source_snapshot, &successor_snapshot] {
        if snapshot.chain_root_id != chain_root_id {
            anyhow::bail!(
                "snapshot {} chain_root_id mismatch: expected {}, got {}",
                snapshot.thread_id,
                chain_root_id,
                snapshot.chain_root_id
            );
        }
    }
    if source_snapshot.thread_id != source_thread_id {
        anyhow::bail!(
            "source snapshot thread_id mismatch: expected {}, got {}",
            source_thread_id,
            source_snapshot.thread_id
        );
    }
    if successor_snapshot.thread_id == source_thread_id
        || successor_snapshot.thread_id == chain_root_id
    {
        anyhow::bail!("continuation successor must be a distinct non-root thread");
    }
    if source_snapshot.status != crate::objects::ThreadStatus::Continued {
        anyhow::bail!("continuation source snapshot must have status continued");
    }
    if successor_snapshot.status != crate::objects::ThreadStatus::Created {
        anyhow::bail!("continuation successor snapshot must have status created");
    }
    if successor_snapshot.upstream_thread_id.as_deref() != Some(source_thread_id) {
        anyhow::bail!(
            "continuation successor {} must name source {} as upstream",
            successor_snapshot.thread_id,
            source_thread_id
        );
    }

    for (event, expected_thread_id, expected_type) in [
        (
            &source_event,
            source_thread_id,
            crate::event_types::THREAD_CONTINUED,
        ),
        (
            &successor_event,
            successor_snapshot.thread_id.as_str(),
            crate::event_types::THREAD_CREATED,
        ),
    ] {
        event.validate()?;
        if event.chain_root_id != chain_root_id {
            anyhow::bail!(
                "event chain_root_id mismatch: expected {}, got {}",
                chain_root_id,
                event.chain_root_id
            );
        }
        if event.thread_id != expected_thread_id {
            anyhow::bail!(
                "continuation event thread_id mismatch: expected {}, got {}",
                expected_thread_id,
                event.thread_id
            );
        }
        if event.event_type != expected_type {
            anyhow::bail!(
                "continuation event type mismatch: expected {}, got {}",
                expected_type,
                event.event_type
            );
        }
    }
    if source_event
        .payload
        .get("successor_thread_id")
        .and_then(serde_json::Value::as_str)
        != Some(successor_snapshot.thread_id.as_str())
    {
        anyhow::bail!(
            "thread_continued successor_thread_id must name {}",
            successor_snapshot.thread_id
        );
    }
    for (field, expected) in [
        ("continuation_from", source_thread_id),
        ("kind", successor_snapshot.kind_name.as_str()),
        ("item_ref", successor_snapshot.item_ref.as_str()),
    ] {
        if successor_event
            .payload
            .get(field)
            .and_then(serde_json::Value::as_str)
            != Some(expected)
        {
            anyhow::bail!(
                "thread_created {field} must be `{expected}` for continuation successor {}",
                successor_snapshot.thread_id
            );
        }
    }

    let _lock = ChainLock::acquire(refs_root, chain_root_id)?;
    let trust_store = crate::signer::trust_store_for_signer(signer);
    let current_chain_state = read_chain_head(
        cas_root,
        refs_root,
        chain_root_id,
        &trust_store,
        head_cache,
    )
    .context("failed to read current chain head")?;
    let current_source = current_chain_state
        .threads
        .get(source_thread_id)
        .ok_or_else(|| {
            anyhow!(
                "source thread {} not found in chain {}",
                source_thread_id,
                chain_root_id
            )
        })?;
    if current_source.status != crate::objects::ThreadStatus::Running {
        anyhow::bail!(
            "continuation source {} must be running, found {}",
            source_thread_id,
            current_source.status
        );
    }
    if current_source.snapshot_hash != expected_source_snapshot_hash {
        anyhow::bail!(
            "continuation source {} changed before commit: expected snapshot {}, found {}",
            source_thread_id,
            expected_source_snapshot_hash,
            current_source.snapshot_hash
        );
    }
    if let Some(existing) = continuation_successor_from_chain(
        cas_root,
        chain_root_id,
        source_thread_id,
        current_source,
    )? {
        anyhow::bail!("thread {source_thread_id} already continued as {existing}");
    }
    if current_chain_state
        .threads
        .contains_key(&successor_snapshot.thread_id)
    {
        anyhow::bail!(
            "thread {} already exists in chain {}",
            successor_snapshot.thread_id,
            chain_root_id
        );
    }

    let mut new_threads = current_chain_state.threads.clone();
    new_threads.insert(
        successor_snapshot.thread_id.clone(),
        ChainThreadEntry {
            snapshot_hash: String::new(),
            last_event_hash: None,
            last_thread_seq: 0,
            status: successor_snapshot.status,
        },
    );

    let mut last_chain_event_hash = current_chain_state.last_event_hash.clone();
    let mut chain_seq = current_chain_state.last_chain_seq + 1;
    let mut stored_events = Vec::with_capacity(2);
    let mut pending_writes = Vec::new();
    for mut event in [source_event, successor_event] {
        let thread_entry = new_threads
            .get_mut(&event.thread_id)
            .expect("validated continuation event thread");
        event.chain_seq = chain_seq;
        event.thread_seq = thread_entry.last_thread_seq + 1;
        event.prev_chain_event_hash = last_chain_event_hash.clone();
        event.prev_thread_event_hash = thread_entry.last_event_hash.clone();
        event.validate()?;

        let event_json = lillux::canonical_json(&event.to_value());
        let event_hash = lillux::sha256_hex(event_json.as_bytes());
        let event_path = lillux::shard_path(cas_root, "objects", &event_hash, ".json");
        pending_writes.push((event_path, event_json.into_bytes()));

        thread_entry.last_event_hash = Some(event_hash.clone());
        thread_entry.last_thread_seq = event.thread_seq;
        last_chain_event_hash = Some(event_hash);
        stored_events.push(event);
        chain_seq += 1;
    }

    let source_event = &stored_events[0];
    let successor_event = &stored_events[1];
    let source_entry = new_threads
        .get(source_thread_id)
        .expect("validated continuation source entry");
    source_snapshot.last_event_hash = source_entry.last_event_hash.clone();
    source_snapshot.last_chain_seq = source_event.chain_seq;
    source_snapshot.last_thread_seq = source_entry.last_thread_seq;
    let successor_entry = new_threads
        .get(&successor_snapshot.thread_id)
        .expect("validated continuation successor entry");
    successor_snapshot.last_event_hash = successor_entry.last_event_hash.clone();
    successor_snapshot.last_chain_seq = successor_event.chain_seq;
    successor_snapshot.last_thread_seq = successor_entry.last_thread_seq;

    let snapshot_object = |snapshot: &ThreadSnapshot| {
        let json = lillux::canonical_json(&snapshot.to_value());
        let hash = lillux::sha256_hex(json.as_bytes());
        let path = lillux::shard_path(cas_root, "objects", &hash, ".json");
        (hash, path, json.into_bytes())
    };
    let (source_snapshot_hash, source_snapshot_path, source_snapshot_json) =
        snapshot_object(&source_snapshot);
    let (successor_snapshot_hash, successor_snapshot_path, successor_snapshot_json) =
        snapshot_object(&successor_snapshot);
    pending_writes.push((source_snapshot_path, source_snapshot_json));
    pending_writes.push((successor_snapshot_path, successor_snapshot_json));
    let source_entry = new_threads
        .get_mut(source_thread_id)
        .expect("validated continuation source entry");
    source_entry.snapshot_hash = source_snapshot_hash;
    source_entry.status = source_snapshot.status;
    new_threads
        .get_mut(&successor_snapshot.thread_id)
        .expect("validated continuation successor entry")
        .snapshot_hash = successor_snapshot_hash.clone();

    let previous_chain_state_hash =
        lillux::sha256_hex(lillux::canonical_json(&current_chain_state.to_value()).as_bytes());
    let new_chain_state = ChainState {
        schema: 1,
        kind: "chain_state".to_string(),
        chain_root_id: chain_root_id.to_string(),
        prev_chain_state_hash: Some(previous_chain_state_hash),
        last_event_hash: last_chain_event_hash,
        last_chain_seq: chain_seq - 1,
        updated_at: lillux::time::iso8601_now(),
        threads: new_threads,
    };
    new_chain_state.validate()?;
    let chain_state_json = lillux::canonical_json(&new_chain_state.to_value());
    let chain_state_hash = lillux::sha256_hex(chain_state_json.as_bytes());
    let chain_state_path =
        lillux::shard_path(cas_root, "objects", &chain_state_hash, ".json");
    pending_writes.push((chain_state_path, chain_state_json.into_bytes()));
    lillux::atomic_write_batch(&pending_writes)
        .context("failed to store continuation handoff objects in CAS")?;

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
    let head_durability = classify_head_write(
        refs::write_signed_ref(&ref_path, signed_ref, signer),
        "failed to write signed continuation head ref",
    )?;

    head_cache.insert(
        chain_root_id.to_string(),
        crate::CachedHead::new(chain_state_hash.clone(), new_chain_state),
    );

    Ok(CommitContinuationResult {
        chain_state_hash,
        source_snapshot,
        successor_snapshot,
        successor_snapshot_hash,
        events: stored_events,
        head_durability,
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
    trust_store: &refs::TrustStore,
    head_cache: &mut HeadCache,
) -> anyhow::Result<ChainState> {
    // Try cache first
    if let Some(cached_head) = head_cache.get(chain_root_id) {
        tracing::trace!(chain_root_id = %chain_root_id, source = "cache", "chain head read");
        return Ok(cached_head.chain_state.clone());
    }

    let verified = load_verified_chain_head(cas_root, refs_root, chain_root_id, trust_store)?;
    let chain_state = verified.chain_state.clone();
    head_cache.insert(chain_root_id.to_string(), verified);
    Ok(chain_state)
}

/// Load an authoritative chain head through its verified ref without using a
/// cache. Startup discovery and projection rebuild use this path so a stale or
/// unverified mutable pointer can never become an authority root.
pub(crate) fn load_verified_chain_head(
    cas_root: &Path,
    refs_root: &Path,
    chain_root_id: &str,
    trust_store: &refs::TrustStore,
) -> anyhow::Result<CachedHead> {
    let ref_path = refs_root
        .join("generic/chains")
        .join(chain_root_id)
        .join("head");

    if !ref_path.exists() {
        anyhow::bail!("signed ref not found for chain {}", chain_root_id);
    }
    tracing::trace!(chain_root_id = %chain_root_id, source = "cas", "chain head read (cache miss)");

    let signed_ref =
        refs::read_verified_ref(&ref_path, trust_store).context("failed to verify signed ref")?;
    let expected_ref_path = format!("chains/{chain_root_id}/head");
    if signed_ref.ref_path != expected_ref_path {
        anyhow::bail!(
            "chain head ref path mismatch: expected {}, got {}",
            expected_ref_path,
            signed_ref.ref_path
        );
    }

    // Load chain_state from CAS
    let cas_path = lillux::shard_path(cas_root, "objects", &signed_ref.target_hash, ".json");
    let chain_state_json =
        std::fs::read_to_string(&cas_path).context("failed to read chain_state from CAS")?;

    let chain_state: ChainState =
        serde_json::from_str(&chain_state_json).context("failed to parse chain_state")?;

    chain_state.validate()?;

    let chain_state_hash =
        lillux::sha256_hex(lillux::canonical_json(&chain_state.to_value()).as_bytes());
    if chain_state_hash != signed_ref.target_hash {
        anyhow::bail!(
            "chain head target hash mismatch: ref names {}, object hashes to {}",
            signed_ref.target_hash,
            chain_state_hash
        );
    }
    if chain_state.chain_root_id != chain_root_id {
        anyhow::bail!(
            "chain head identity mismatch: expected {}, got {}",
            chain_root_id,
            chain_state.chain_root_id
        );
    }
    if chain_state.updated_at != signed_ref.updated_at {
        anyhow::bail!(
            "chain head timestamp mismatch: ref has {}, object has {}",
            signed_ref.updated_at,
            chain_state.updated_at
        );
    }

    Ok(CachedHead::new(chain_state_hash, chain_state))
}

/// Read a thread's snapshot from a chain.
///
/// Returns None if the thread doesn't exist in the chain.
pub fn read_thread_snapshot(
    cas_root: &Path,
    refs_root: &Path,
    chain_root_id: &str,
    thread_id: &str,
    trust_store: &refs::TrustStore,
    head_cache: &mut HeadCache,
) -> anyhow::Result<ReadSnapshotResult> {
    let chain_state = read_chain_head(
        cas_root,
        refs_root,
        chain_root_id,
        trust_store,
        head_cache,
    )?;

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
    let cas_path = lillux::shard_path(cas_root, "objects", &thread_entry.snapshot_hash, ".json");
    let snapshot_json =
        std::fs::read_to_string(&cas_path).context("failed to read snapshot from CAS")?;

    let snapshot: ThreadSnapshot =
        serde_json::from_str(&snapshot_json).context("failed to parse snapshot")?;

    snapshot.validate()?;
    let snapshot_hash =
        lillux::sha256_hex(lillux::canonical_json(&snapshot.to_value()).as_bytes());
    if snapshot_hash != thread_entry.snapshot_hash {
        anyhow::bail!(
            "thread snapshot hash mismatch: chain entry names {}, object hashes to {}",
            thread_entry.snapshot_hash,
            snapshot_hash
        );
    }
    if snapshot.thread_id != thread_id {
        anyhow::bail!(
            "thread snapshot identity mismatch: expected {}, got {}",
            thread_id,
            snapshot.thread_id
        );
    }
    if snapshot.chain_root_id != chain_root_id {
        anyhow::bail!(
            "thread snapshot chain mismatch: expected {}, got {}",
            chain_root_id,
            snapshot.chain_root_id
        );
    }
    if snapshot.status != thread_entry.status {
        anyhow::bail!(
            "thread snapshot status mismatch: chain entry has {}, snapshot has {}",
            thread_entry.status.as_str(),
            snapshot.status.as_str()
        );
    }
    validate_authoritative_snapshot_cursor(
        cas_root,
        &chain_state,
        thread_id,
        thread_entry,
        &snapshot,
    )?;

    Ok(ReadSnapshotResult {
        snapshot: Some(snapshot),
        chain_state_hash,
    })
}

fn validate_authoritative_snapshot_cursor(
    cas_root: &Path,
    chain_state: &ChainState,
    thread_id: &str,
    entry: &ChainThreadEntry,
    snapshot: &ThreadSnapshot,
) -> anyhow::Result<()> {
    if snapshot.last_thread_seq > entry.last_thread_seq {
        anyhow::bail!(
            "thread snapshot sequence {} exceeds chain entry sequence {}",
            snapshot.last_thread_seq,
            entry.last_thread_seq
        );
    }
    if snapshot.last_chain_seq > chain_state.last_chain_seq {
        anyhow::bail!(
            "thread snapshot chain sequence {} exceeds chain head sequence {}",
            snapshot.last_chain_seq,
            chain_state.last_chain_seq
        );
    }
    match snapshot.last_event_hash.as_deref() {
        None if snapshot.last_thread_seq == 0 && snapshot.last_chain_seq == 0 => {}
        None => anyhow::bail!("thread snapshot has non-zero cursors without a last event"),
        Some(_) if snapshot.last_thread_seq == 0 || snapshot.last_chain_seq == 0 => {
            anyhow::bail!("thread snapshot has a last event with zero sequence")
        }
        Some(_) => {}
    }
    match entry.last_event_hash.as_deref() {
        None if entry.last_thread_seq == 0 => {
            if snapshot.last_event_hash.is_some() {
                anyhow::bail!("thread snapshot event is not reachable from an empty chain entry");
            }
            return Ok(());
        }
        None => anyhow::bail!("chain entry has non-zero sequence without a last event"),
        Some(_) if entry.last_thread_seq == 0 => {
            anyhow::bail!("chain entry has a last event with zero sequence")
        }
        Some(_) => {}
    }

    let target_hash = match snapshot.last_event_hash.as_deref() {
        Some(hash) => hash,
        None => return Ok(()),
    };
    let mut current_hash = entry.last_event_hash.clone();
    let mut expected_seq = entry.last_thread_seq;
    let mut visited = HashSet::new();
    while let Some(event_hash) = current_hash {
        if !visited.insert(event_hash.clone()) {
            anyhow::bail!("cycle in thread event chain for {thread_id}");
        }
        let path = lillux::shard_path(cas_root, "objects", &event_hash, ".json");
        let json = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read thread event {event_hash} from CAS"))?;
        let event: ThreadEvent = serde_json::from_str(&json)
            .with_context(|| format!("failed to parse thread event {event_hash}"))?;
        event.validate()?;
        let actual_hash = crate::objects::thread_event::hash_event(&event);
        if actual_hash != event_hash {
            anyhow::bail!(
                "thread event hash mismatch: named {event_hash}, object hashes to {actual_hash}"
            );
        }
        if event.chain_root_id != chain_state.chain_root_id
            || event.thread_id != thread_id
            || event.thread_seq != expected_seq
            || event.chain_seq > chain_state.last_chain_seq
        {
            anyhow::bail!("thread event cursor identity or sequence mismatch at {event_hash}");
        }
        if event_hash == target_hash {
            if event.thread_seq != snapshot.last_thread_seq
                || event.chain_seq != snapshot.last_chain_seq
            {
                anyhow::bail!("thread snapshot cursors do not match its last event");
            }
            return Ok(());
        }
        current_hash = event.prev_thread_event_hash;
        expected_seq = expected_seq
            .checked_sub(1)
            .ok_or_else(|| anyhow!("thread event sequence underflow for {thread_id}"))?;
    }
    anyhow::bail!("thread snapshot last event is not reachable from chain entry")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signer::TestSigner;
    use ryeos_tracing::test;

    #[test]
    fn durability_uncertain_head_is_committed_not_failed() {
        let durability = classify_head_write(
            Err(lillux::AtomicMutationError::DurabilityUncertain(anyhow::anyhow!(
                "directory sync failed"
            ))),
            "writing test head",
        )
        .expect("visible rename is a committed head");
        assert!(matches!(durability, HeadDurability::Uncertain(_)));
    }

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
        let snapshot_path =
            lillux::shard_path(&cas_root, "objects", &result.snapshot_hash, ".json");
        assert!(snapshot_path.exists());

        // Verify chain_state object exists in CAS
        let chain_state_path =
            lillux::shard_path(&cas_root, "objects", &result.chain_state_hash, ".json");
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
            AppendEventsInput {
                cas_root: &cas_root,
                refs_root: &refs_root,
                chain_root_id: "T-root",
                thread_id: "T-root",
                events: vec![event],
                snapshot_updates: vec![],
                signer: &signer,
            },
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
            AppendEventsInput {
                cas_root: &cas_root,
                refs_root: &refs_root,
                chain_root_id: "T-root",
                thread_id: "T-root",
                events: vec![event],
                snapshot_updates: vec![],
                signer: &signer,
            },
            &mut head_cache,
        )
        .unwrap();

        // Verify new chain_state exists in CAS
        let chain_state_path =
            lillux::shard_path(&cas_root, "objects", &result.chain_state_hash, ".json");
        assert!(chain_state_path.exists());
    }

    #[test]
    fn add_thread_with_events_advances_one_head_with_child_events() {
        use crate::objects::thread_snapshot::ThreadSnapshotBuilder;

        let tempdir = tempfile::tempdir().unwrap();
        let cas_root = tempdir.path().join("state");
        let refs_root = tempdir.path().join("refs");
        let signer = TestSigner::default();
        let mut head_cache = HeadCache::new();

        create_chain(
            &cas_root,
            &refs_root,
            "T-root",
            make_test_snapshot("T-root"),
            &signer,
            &mut head_cache,
        )
        .unwrap();

        let child_snapshot = ThreadSnapshotBuilder::new(
            "T-child",
            "T-root",
            "directive",
            "directive:test",
            "native:test",
        )
        .build();
        let create_event = make_test_event("T-root", "T-child");
        let edge_event = make_test_event("T-root", "T-child");
        let result = add_thread_to_chain_with_events(
            &cas_root,
            &refs_root,
            "T-root",
            child_snapshot,
            vec![create_event, edge_event],
            &signer,
            &mut head_cache,
        )
        .unwrap();

        assert_eq!(result.first_chain_seq, 1);
        assert_eq!(result.event_count, 2);
        assert_eq!(result.events[0].chain_seq, 1);
        assert_eq!(result.events[0].thread_seq, 1);
        assert!(result.events[0].prev_thread_event_hash.is_none());
        assert_eq!(result.events[1].chain_seq, 2);
        assert_eq!(result.events[1].thread_seq, 2);
        assert_eq!(
            result.events[1].prev_thread_event_hash,
            Some(lillux::sha256_hex(
                lillux::canonical_json(&result.events[0].to_value()).as_bytes()
            ))
        );

        let trust = crate::signer::trust_store_for_signer(&signer);
        let head =
            read_chain_head(&cas_root, &refs_root, "T-root", &trust, &mut head_cache).unwrap();
        let child = head.threads.get("T-child").expect("child thread entry");
        assert_eq!(head.last_chain_seq, 2);
        assert_eq!(child.last_thread_seq, 2);
        assert_eq!(child.last_event_hash, head.last_event_hash);
    }

    #[test]
    fn continuation_handoff_advances_one_head_with_both_threads_and_events() {
        use crate::objects::thread_event::NewEvent;
        use crate::objects::thread_snapshot::{ThreadSnapshotBuilder, ThreadStatus};

        let tempdir = tempfile::tempdir().unwrap();
        let cas_root = tempdir.path().join("state");
        let refs_root = tempdir.path().join("refs");
        let signer = TestSigner::default();
        let mut head_cache = HeadCache::new();

        let running = ThreadSnapshotBuilder::new(
            "T-root",
            "T-root",
            "directive",
            "system/test",
            "directive-runtime",
        )
        .status(ThreadStatus::Running)
        .build();
        let initial = create_chain(
            &cas_root,
            &refs_root,
            "T-root",
            running,
            &signer,
            &mut head_cache,
        )
        .unwrap();
        let continued = ThreadSnapshotBuilder::new(
            "T-root",
            "T-root",
            "directive",
            "system/test",
            "directive-runtime",
        )
        .status(ThreadStatus::Continued)
        .build();
        let successor = ThreadSnapshotBuilder::new(
            "T-next",
            "T-root",
            "directive",
            "system/test",
            "directive-runtime",
        )
        .upstream_thread_id(Some("T-root".to_string()))
        .build();
        let source_event = NewEvent::new(
            "T-root",
            "T-root",
            crate::event_types::THREAD_CONTINUED,
        )
        .payload(serde_json::json!({"successor_thread_id": "T-next"}))
        .build();
        let successor_event = NewEvent::new(
            "T-root",
            "T-next",
            crate::event_types::THREAD_CREATED,
        )
        .payload(serde_json::json!({
            "continuation_from": "T-root",
            "kind": "directive",
            "item_ref": "system/test"
        }))
        .build();

        let committed = commit_continuation(
            CommitContinuationInput {
                cas_root: &cas_root,
                refs_root: &refs_root,
                chain_root_id: "T-root",
                source_thread_id: "T-root",
                expected_source_snapshot_hash: &initial.snapshot_hash,
                source_snapshot: continued,
                successor_snapshot: successor,
                source_event,
                successor_event,
                signer: &signer,
            },
            &mut head_cache,
        )
        .unwrap();

        assert_ne!(committed.chain_state_hash, initial.chain_state_hash);
        assert_eq!(committed.events.len(), 2);
        assert_eq!(committed.events[0].chain_seq, 1);
        assert_eq!(committed.events[0].thread_seq, 1);
        assert_eq!(committed.events[1].chain_seq, 2);
        assert_eq!(committed.events[1].thread_seq, 1);
        assert_eq!(
            committed.events[1].prev_chain_event_hash,
            Some(lillux::sha256_hex(
                lillux::canonical_json(&committed.events[0].to_value()).as_bytes()
            ))
        );
        assert!(committed.events[1].prev_thread_event_hash.is_none());
        assert_eq!(
            committed.source_snapshot.last_event_hash,
            Some(lillux::sha256_hex(
                lillux::canonical_json(&committed.events[0].to_value()).as_bytes()
            ))
        );
        assert_eq!(committed.source_snapshot.last_thread_seq, 1);
        assert_eq!(committed.source_snapshot.last_chain_seq, 1);
        assert_eq!(
            committed.successor_snapshot.last_event_hash,
            Some(lillux::sha256_hex(
                lillux::canonical_json(&committed.events[1].to_value()).as_bytes()
            ))
        );
        assert_eq!(committed.successor_snapshot.last_thread_seq, 1);
        assert_eq!(committed.successor_snapshot.last_chain_seq, 2);

        let trust = crate::signer::trust_store_for_signer(&signer);
        let head =
            read_chain_head(&cas_root, &refs_root, "T-root", &trust, &mut head_cache).unwrap();
        assert_eq!(head.prev_chain_state_hash, Some(initial.chain_state_hash));
        assert_eq!(head.last_chain_seq, 2);
        assert_eq!(
            head.threads["T-root"].status,
            crate::objects::ThreadStatus::Continued
        );
        assert_eq!(
            head.threads["T-next"].status,
            crate::objects::ThreadStatus::Created
        );
        assert_eq!(head.threads["T-root"].last_thread_seq, 1);
        assert_eq!(head.threads["T-next"].last_thread_seq, 1);
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
        let trust = crate::signer::trust_store_for_signer(&signer);
        let head =
            read_chain_head(&cas_root, &refs_root, "T-root", &trust, &mut head_cache).unwrap();
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
        let trust = crate::signer::trust_store_for_signer(&signer);
        let result = read_thread_snapshot(
            &cas_root,
            &refs_root,
            "T-root",
            "T-root",
            &trust,
            &mut head_cache,
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
            &crate::signer::trust_store_for_signer(&signer),
            &mut head_cache,
        )
        .unwrap();

        assert!(result.snapshot.is_none());
    }

    #[test]
    fn authoritative_head_rejects_tampered_signature() {
        let tempdir = tempfile::tempdir().unwrap();
        let cas_root = tempdir.path().join("state");
        let refs_root = tempdir.path().join("refs");
        let signer = TestSigner::default();
        let mut head_cache = HeadCache::new();
        create_chain(
            &cas_root,
            &refs_root,
            "T-root",
            make_test_snapshot("T-root"),
            &signer,
            &mut head_cache,
        )
        .unwrap();

        let head_path = refs_root.join("generic/chains/T-root/head");
        let mut head: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&head_path).unwrap()).unwrap();
        head["signature"] = serde_json::Value::String("AAAA".to_owned());
        lillux::atomic_write(&head_path, lillux::canonical_json(&head).as_bytes()).unwrap();
        head_cache.clear();

        let error = read_chain_head(
            &cas_root,
            &refs_root,
            "T-root",
            &crate::signer::trust_store_for_signer(&signer),
            &mut head_cache,
        )
        .unwrap_err();
        assert!(error.to_string().contains("verify signed ref"));
    }

    #[test]
    fn authoritative_snapshot_rejects_content_hash_mismatch() {
        let tempdir = tempfile::tempdir().unwrap();
        let cas_root = tempdir.path().join("state");
        let refs_root = tempdir.path().join("refs");
        let signer = TestSigner::default();
        let mut head_cache = HeadCache::new();
        create_chain(
            &cas_root,
            &refs_root,
            "T-root",
            make_test_snapshot("T-root"),
            &signer,
            &mut head_cache,
        )
        .unwrap();
        let snapshot_hash = head_cache.get("T-root").unwrap().chain_state.threads["T-root"]
            .snapshot_hash
            .clone();
        let snapshot_path = lillux::shard_path(&cas_root, "objects", &snapshot_hash, ".json");
        let replacement = make_test_snapshot("T-other");
        lillux::atomic_write(
            &snapshot_path,
            lillux::canonical_json(&replacement.to_value()).as_bytes(),
        )
        .unwrap();

        let error = read_thread_snapshot(
            &cas_root,
            &refs_root,
            "T-root",
            "T-root",
            &crate::signer::trust_store_for_signer(&signer),
            &mut head_cache,
        )
        .unwrap_err();
        assert!(error.to_string().contains("snapshot hash mismatch"));
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
            AppendEventsInput {
                cas_root: &cas_root,
                refs_root: &refs_root,
                chain_root_id: "T-root",
                thread_id: "T-root",
                events: vec![],
                snapshot_updates: vec![],
                signer: &signer,
            },
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
            AppendEventsInput {
                cas_root: &cas_root,
                refs_root: &refs_root,
                chain_root_id: "T-root",
                thread_id: "T-root",
                events: vec![event],
                snapshot_updates: vec![],
                signer: &signer,
            },
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
            AppendEventsInput {
                cas_root: &cas_root,
                refs_root: &refs_root,
                chain_root_id: "T-root",
                thread_id: "T-root",
                events: vec![event],
                snapshot_updates: vec![],
                signer: &signer,
            },
            &mut head_cache,
        );

        assert!(result.is_err());
    }

    #[test]
    fn add_thread_to_chain_succeeds() {
        let tempdir = tempfile::tempdir().unwrap();
        let cas_root = tempdir.path().join("state");
        let refs_root = tempdir.path().join("refs");
        let signer = TestSigner::default();
        let mut head_cache = HeadCache::new();

        // Create root chain
        let root_snapshot = make_test_snapshot("T-root");
        create_chain(
            &cas_root,
            &refs_root,
            "T-root",
            root_snapshot,
            &signer,
            &mut head_cache,
        )
        .unwrap();

        // Add child thread
        use crate::objects::thread_snapshot::ThreadSnapshotBuilder;
        let child_snapshot = ThreadSnapshotBuilder::new(
            "T-child",
            "T-root",
            "tool",
            "test/child",
            "native:tool-runtime",
        )
        .build();

        let result = add_thread_to_chain(
            &cas_root,
            &refs_root,
            "T-root",
            child_snapshot,
            &signer,
            &mut head_cache,
        );

        assert!(result.is_ok());
        let result = result.unwrap();
        assert!(!result.snapshot_hash.is_empty());
        assert!(!result.chain_state_hash.is_empty());
    }

    #[test]
    fn add_thread_to_chain_rejects_duplicate_thread() {
        let tempdir = tempfile::tempdir().unwrap();
        let cas_root = tempdir.path().join("state");
        let refs_root = tempdir.path().join("refs");
        let signer = TestSigner::default();
        let mut head_cache = HeadCache::new();

        // Create root chain
        let root_snapshot = make_test_snapshot("T-root");
        create_chain(
            &cas_root,
            &refs_root,
            "T-root",
            root_snapshot,
            &signer,
            &mut head_cache,
        )
        .unwrap();

        // Try to add root thread again
        let duplicate_snapshot = make_test_snapshot("T-root");
        let result = add_thread_to_chain(
            &cas_root,
            &refs_root,
            "T-root",
            duplicate_snapshot,
            &signer,
            &mut head_cache,
        );

        assert!(result.is_err());
    }

    #[test]
    fn add_thread_to_chain_rejects_root_thread() {
        let tempdir = tempfile::tempdir().unwrap();
        let cas_root = tempdir.path().join("state");
        let refs_root = tempdir.path().join("refs");
        let signer = TestSigner::default();
        let mut head_cache = HeadCache::new();

        // Create root chain
        let root_snapshot = make_test_snapshot("T-root");
        create_chain(
            &cas_root,
            &refs_root,
            "T-root",
            root_snapshot,
            &signer,
            &mut head_cache,
        )
        .unwrap();

        // Try to add a root thread (thread_id == chain_root_id)
        let root_snapshot = make_test_snapshot("T-root");
        let result = add_thread_to_chain(
            &cas_root,
            &refs_root,
            "T-root",
            root_snapshot,
            &signer,
            &mut head_cache,
        );

        assert!(result.is_err());
    }

    // ── Trace-capture tests ──────────────────────────────────────

    #[test]
    fn create_chain_emits_span() {
        let tempdir = tempfile::tempdir().unwrap();
        let cas_root = tempdir.path().join("state");
        let refs_root = tempdir.path().join("refs");
        let signer = TestSigner::default();
        let mut head_cache = HeadCache::new();
        let snapshot = make_test_snapshot("T-trace");

        let (_, spans) = test::capture_traces(|| {
            let _ = create_chain(
                &cas_root,
                &refs_root,
                "T-trace",
                snapshot,
                &signer,
                &mut head_cache,
            );
        });

        let span = test::find_span(&spans, "state:chain_create");
        assert!(
            span.is_some(),
            "expected state:chain_create span, got: {:?}",
            spans.iter().map(|s| &s.name).collect::<Vec<_>>()
        );

        let span = span.unwrap();
        let field_val = |name: &str| -> Option<&str> {
            span.fields
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(field_val("chain_root_id"), Some("T-trace"));
    }

    #[test]
    fn append_events_emits_span() {
        let tempdir = tempfile::tempdir().unwrap();
        let cas_root = tempdir.path().join("state");
        let refs_root = tempdir.path().join("refs");
        let signer = TestSigner::default();
        let mut head_cache = HeadCache::new();

        let snapshot = make_test_snapshot("T-trace2");
        create_chain(
            &cas_root,
            &refs_root,
            "T-trace2",
            snapshot,
            &signer,
            &mut head_cache,
        )
        .unwrap();

        let event = make_test_event("T-trace2", "T-trace2");
        let (_, spans) = test::capture_traces(|| {
            let _ = append_events(
                AppendEventsInput {
                    cas_root: &cas_root,
                    refs_root: &refs_root,
                    chain_root_id: "T-trace2",
                    thread_id: "T-trace2",
                    events: vec![event],
                    snapshot_updates: vec![],
                    signer: &signer,
                },
                &mut head_cache,
            );
        });

        let span = test::find_span(&spans, "state:chain_append");
        assert!(
            span.is_some(),
            "expected state:chain_append span, got: {:?}",
            spans.iter().map(|s| &s.name).collect::<Vec<_>>()
        );

        let span = span.unwrap();
        let field_val = |name: &str| -> Option<&str> {
            span.fields
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(field_val("chain_root_id"), Some("T-trace2"));
        assert_eq!(field_val("thread_id"), Some("T-trace2"));
        assert_eq!(field_val("event_count"), Some("1"));
    }
}
