//! Chain operations — create, append, read, lock.
//!
//! Every execution chain is a hash-linked sequence of immutable CAS objects.
//! All writes are protected by file locks and validated against the signed head ref.

use std::collections::BTreeMap;
use std::fs::File;
use std::path::Path;

use anyhow::{anyhow, Context};
use serde::de::DeserializeOwned;

use crate::objects::{ChainState, ChainThreadEntry, ThreadEvent, ThreadSnapshot};
use crate::signer::Signer;
use crate::{refs, CachedHead, HeadCache, SignedRef, TrustStore};

pub use self::types::*;

mod validation;

pub(crate) use validation::{validate_chain_positions, validate_stored_snapshot};

/// Validate through an already-pinned CAS root so every object in the closure
/// is read from the same authority inode.
pub(crate) fn validate_authoritative_history_with_cas(
    cas: &lillux::CasStore,
    chain_root_id: &str,
    target_hash: &str,
    expected_ancestor: Option<&str>,
) -> anyhow::Result<()> {
    validation::validate_authoritative_history_with_cas(
        cas,
        chain_root_id,
        target_hash,
        expected_ancestor,
    )
}

pub(crate) fn validate_authoritative_history_with_cas_and_check(
    cas: &lillux::CasStore,
    chain_root_id: &str,
    target_hash: &str,
    expected_ancestor: Option<&str>,
    check: &mut dyn FnMut() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    validation::validate_authoritative_history_with_cas_and_check(
        cas,
        chain_root_id,
        target_hash,
        expected_ancestor,
        check,
    )
}

mod types {
    use crate::objects::{ThreadEvent, ThreadSnapshot};

    /// Result of creating a new execution chain.
    #[derive(Debug, Clone)]
    pub struct CreateResult {
        /// CAS hash of the initial chain state.
        pub chain_state_hash: String,
        /// CAS hash of the initial thread snapshot.
        pub snapshot_hash: String,
        /// Exact normalized snapshot committed under `snapshot_hash`.
        pub snapshot: ThreadSnapshot,
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
        /// Exact normalized/stamped snapshot updates committed by this append.
        pub snapshot_updates: Vec<SnapshotUpdate>,
    }

    /// Result of adding a thread and its initial events in one chain update.
    #[derive(Debug, Clone)]
    pub struct AddThreadWithEventsResult {
        /// CAS hash of the new chain state.
        pub chain_state_hash: String,
        /// CAS hash of the new thread snapshot.
        pub snapshot_hash: String,
        /// Exact normalized snapshot committed under `snapshot_hash`.
        pub snapshot: ThreadSnapshot,
        /// Sequence number of the first appended event.
        pub first_chain_seq: u64,
        /// Number of events appended.
        pub event_count: usize,
        /// The events that were persisted.
        pub events: Vec<ThreadEvent>,
        /// Exact normalized/stamped updates committed for existing threads.
        pub snapshot_updates: Vec<SnapshotUpdate>,
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

/// File lock for a chain.
pub struct ChainLock {
    /// Held only for RAII; Drop releases the file lock via closed fd.
    _lock_file: File,
    /// Pins the exact chain directory in which the lock was acquired.
    lock_directory: lillux::PinnedDirectory,
    /// Retains the exact refs-root ancestor when the owner already has one.
    _refs_directory: Option<lillux::PinnedDirectory>,
    /// Retains the exact CAS root used by the same state-store authority.
    cas_directory: Option<lillux::PinnedDirectory>,
    recovery: Option<crate::recovery::RecoveryStore>,
    chain_root_id: String,
}

impl ChainLock {
    /// Acquire exclusive lock for a chain using flock.
    ///
    /// Creates a lock file under `runtime_state_dir/objects/refs/generic/chains/<chain_root_id>/lock`
    /// and acquires an exclusive file lock via flock(2).
    pub fn acquire(refs_root: &Path, chain_root_id: &str) -> anyhow::Result<Self> {
        Self::acquire_inner(refs_root, chain_root_id, true)
    }

    /// Acquire an already-established chain lock without creating directories
    /// or the lock anchor. Read-only verification and dry-run maintenance use
    /// this so inspection never mutates authority state.
    pub fn acquire_existing(refs_root: &Path, chain_root_id: &str) -> anyhow::Result<Self> {
        Self::acquire_inner(refs_root, chain_root_id, false)
    }

    pub(crate) fn acquire_in_refs_directory(
        refs_directory: &lillux::PinnedDirectory,
        cas_directory: &lillux::PinnedDirectory,
        recovery: &crate::recovery::RecoveryStore,
        chain_root_id: &str,
    ) -> anyhow::Result<Self> {
        Self::acquire_in_refs_directory_inner(
            refs_directory,
            cas_directory,
            recovery,
            chain_root_id,
            true,
        )
    }

    pub(crate) fn acquire_existing_in_refs_directory(
        refs_directory: &lillux::PinnedDirectory,
        cas_directory: &lillux::PinnedDirectory,
        recovery: &crate::recovery::RecoveryStore,
        chain_root_id: &str,
    ) -> anyhow::Result<Self> {
        Self::acquire_in_refs_directory_inner(
            refs_directory,
            cas_directory,
            recovery,
            chain_root_id,
            false,
        )
    }

    fn acquire_in_refs_directory_inner(
        refs_directory: &lillux::PinnedDirectory,
        cas_directory: &lillux::PinnedDirectory,
        recovery: &crate::recovery::RecoveryStore,
        chain_root_id: &str,
        create_if_missing: bool,
    ) -> anyhow::Result<Self> {
        validate_chain_root_id(chain_root_id)?;
        let open_child = |parent: &lillux::PinnedDirectory,
                          name: &std::ffi::OsStr,
                          label: &str|
         -> anyhow::Result<lillux::PinnedDirectory> {
            if create_if_missing {
                parent
                    .open_or_create_child(name, 0o777)
                    .with_context(|| label.to_string())
            } else {
                parent
                    .open_child_directory(name)?
                    .ok_or_else(|| anyhow!("{label}: directory is absent"))
            }
        };
        let generic = open_child(
            refs_directory,
            std::ffi::OsStr::new("generic"),
            "open generic refs namespace through pinned refs root",
        )?;
        let chains = open_child(
            &generic,
            std::ffi::OsStr::new("chains"),
            "open chain refs namespace through pinned refs root",
        )?;
        let lock_directory = open_child(
            &chains,
            std::ffi::OsStr::new(chain_root_id),
            "open chain lock directory through pinned refs root",
        )?;
        Self::acquire_in_lock_directory(
            lock_directory,
            Some(refs_directory.try_clone()?),
            Some(cas_directory.try_clone()?),
            Some(recovery.clone()),
            chain_root_id,
            create_if_missing,
        )
    }

    fn acquire_inner(
        refs_root: &Path,
        chain_root_id: &str,
        create_if_missing: bool,
    ) -> anyhow::Result<Self> {
        validate_chain_root_id(chain_root_id)?;
        let lock_directory_path = refs_root.join("generic/chains").join(chain_root_id);
        let lock_directory = if create_if_missing {
            lillux::PinnedDirectory::open_or_create(&lock_directory_path)
                .context("failed to establish no-follow chain lock directory")?
        } else {
            lillux::PinnedDirectory::open(&lock_directory_path)?
                .ok_or_else(|| anyhow!("chain lock directory is absent"))?
        };
        Self::acquire_in_lock_directory(
            lock_directory,
            None,
            None,
            None,
            chain_root_id,
            create_if_missing,
        )
    }

    fn acquire_in_lock_directory(
        lock_directory: lillux::PinnedDirectory,
        refs_directory: Option<lillux::PinnedDirectory>,
        cas_directory: Option<lillux::PinnedDirectory>,
        recovery: Option<crate::recovery::RecoveryStore>,
        chain_root_id: &str,
        create_if_missing: bool,
    ) -> anyhow::Result<Self> {
        let lock_name = std::ffi::OsStr::new("lock");
        let lock_file = if create_if_missing {
            let file = lock_directory
                .open_regular_create(lock_name, true, false, 0o600)
                .context("failed to open chain lock file")?;
            lock_directory.sync()?;
            file
        } else {
            lock_directory
                .open_regular(lock_name, true)?
                .ok_or_else(|| anyhow!("chain lock anchor is absent"))?
        };

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
            lock_directory,
            _refs_directory: refs_directory,
            cas_directory,
            recovery,
            chain_root_id: chain_root_id.to_string(),
        })
    }

    /// Get the chain root ID this lock protects.
    pub fn chain_root_id(&self) -> &str {
        &self.chain_root_id
    }

    /// Prove both the chain identity and the concrete refs-root lock inode.
    /// A same-named lock acquired in another node root is not authority for
    /// this chain's journal or head.
    pub(crate) fn ensure_protects(
        &self,
        refs_root: &Path,
        chain_root_id: &str,
    ) -> anyhow::Result<()> {
        ensure_lock_protects(self, chain_root_id)?;
        let expected_directory_path = refs_root.join("generic/chains").join(chain_root_id);
        let expected_directory = lillux::PinnedDirectory::open(&expected_directory_path)?
            .ok_or_else(|| anyhow!("chain lock directory disappeared"))?;
        if !self.lock_directory.is_same_directory(&expected_directory)? {
            anyhow::bail!("chain lock directory for {chain_root_id} was replaced");
        }
        let expected_path = expected_directory_path.join("lock");
        let expected = expected_directory
            .open_regular(std::ffi::OsStr::new("lock"), false)?
            .ok_or_else(|| anyhow!("chain lock anchor disappeared"))?
            .metadata()
            .with_context(|| format!("inspect expected chain lock {}", expected_path.display()))?;
        let held = self
            ._lock_file
            .metadata()
            .context("inspect held chain lock")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if expected.dev() != held.dev() || expected.ino() != held.ino() {
                anyhow::bail!("chain lock for {chain_root_id} belongs to a different refs root");
            }
        }
        #[cfg(not(unix))]
        anyhow::bail!("chain locking is unavailable on this platform");
        Ok(())
    }

    /// Read the signed head from the exact chain-directory inode that carries
    /// this lock, so pathname replacement cannot rebind head authority.
    pub(crate) fn read_verified_head(
        &self,
        trust_store: &TrustStore,
    ) -> anyhow::Result<Option<SignedRef>> {
        let head = refs::read_verified_ref_in_directory(
            &self.lock_directory,
            std::ffi::OsStr::new("head"),
            trust_store,
        )?;
        if let Some(head) = head.as_ref() {
            let expected = format!("chains/{}/head", self.chain_root_id);
            if head.ref_path != expected {
                anyhow::bail!(
                    "chain head ref_path mismatch: expected {expected}, got {}",
                    head.ref_path
                );
            }
        }
        Ok(head)
    }

    pub(crate) fn write_signed_head(
        &self,
        signed_ref: SignedRef,
        signer: &dyn Signer,
    ) -> anyhow::Result<()> {
        refs::write_signed_ref_in_directory(
            &self.lock_directory,
            std::ffi::OsStr::new("head"),
            signed_ref,
            signer,
        )
    }

    pub(crate) fn remove_head(&self) -> anyhow::Result<bool> {
        refs::remove_ref_in_directory(&self.lock_directory, std::ffi::OsStr::new("head"))
    }

    fn recovery_store(&self) -> Option<&crate::recovery::RecoveryStore> {
        self.recovery.as_ref()
    }

    fn cas_directory(&self) -> Option<&lillux::PinnedDirectory> {
        self.cas_directory.as_ref()
    }

    fn ensure_cas_root(&self, cas_root: &Path) -> anyhow::Result<()> {
        let Some(held) = self.cas_directory() else {
            return Ok(());
        };
        let current = lillux::PinnedDirectory::open(cas_root)?
            .ok_or_else(|| anyhow!("CAS root disappeared: {}", cas_root.display()))?;
        if !held.is_same_directory(&current)? {
            anyhow::bail!("CAS root was replaced: {}", cas_root.display());
        }
        Ok(())
    }
}

fn ensure_lock_protects(lock: &ChainLock, chain_root_id: &str) -> anyhow::Result<()> {
    if lock.chain_root_id() != chain_root_id {
        anyhow::bail!(
            "chain lock protects {}, not {chain_root_id}",
            lock.chain_root_id()
        );
    }
    Ok(())
}

fn validate_chain_root_id(chain_root_id: &str) -> anyhow::Result<()> {
    if chain_root_id.is_empty()
        || chain_root_id.len() > 255
        || matches!(chain_root_id, "." | "..")
        || chain_root_id.contains(['/', '\\'])
        || chain_root_id.bytes().any(|byte| byte.is_ascii_control())
    {
        anyhow::bail!("invalid chain root id: {chain_root_id:?}");
    }
    Ok(())
}

fn read_current_signed_chain_head(
    refs_root: &Path,
    chain_root_id: &str,
    trust_store: &TrustStore,
) -> anyhow::Result<Option<SignedRef>> {
    refs::read_verified_generic_head_ref(refs_root, "chains", chain_root_id, trust_store)
        .context("failed to read current signed chain head")
}

fn verify_expected_current_head(
    chain_lock: &ChainLock,
    chain_root_id: &str,
    expected_previous_hash: Option<&str>,
    trust_store: &TrustStore,
) -> anyhow::Result<()> {
    ensure_lock_protects(chain_lock, chain_root_id)?;
    let current = chain_lock.read_verified_head(trust_store)?;
    match (current.as_ref(), expected_previous_hash) {
        (None, None) => Ok(()),
        (Some(_), None) => anyhow::bail!("chain {chain_root_id} already has a signed head"),
        (None, Some(expected)) => {
            anyhow::bail!("chain {chain_root_id} head disappeared: expected {expected}")
        }
        (Some(current), Some(expected)) if current.target_hash == expected => Ok(()),
        (Some(current), Some(expected)) => anyhow::bail!(
            "chain {chain_root_id} head conflict: expected {expected}, got {}",
            current.target_hash
        ),
    }
}

fn cas_store_for_lock(
    cas_root: &Path,
    chain_lock: Option<&ChainLock>,
) -> anyhow::Result<lillux::CasStore> {
    if let Some(chain_lock) = chain_lock {
        chain_lock.ensure_cas_root(cas_root)?;
    }
    match chain_lock.and_then(ChainLock::cas_directory) {
        Some(directory) => Ok(lillux::CasStore::from_pinned_root(directory.try_clone()?)),
        None => Ok(lillux::CasStore::new(cas_root.to_path_buf())),
    }
}

fn read_cas_object<T: DeserializeOwned>(
    cas_root: &Path,
    chain_lock: Option<&ChainLock>,
    hash: &str,
    label: &str,
) -> anyhow::Result<T> {
    let value = cas_store_for_lock(cas_root, chain_lock)?
        .get_object(hash)
        .with_context(|| format!("read {label} CAS object {hash}"))?
        .ok_or_else(|| anyhow!("{label} CAS object is absent: {hash}"))?;
    serde_json::from_value(value).with_context(|| format!("decode {label} CAS object {hash}"))
}

fn write_cas_batch(
    cas_root: &Path,
    chain_lock: &ChainLock,
    writes: &[(std::path::PathBuf, Vec<u8>)],
) -> anyhow::Result<()> {
    chain_lock.ensure_cas_root(cas_root)?;
    match chain_lock.cas_directory() {
        Some(directory) => lillux::atomic_write_batch_in_pinned_root(directory, writes),
        None => lillux::atomic_write_batch(writes),
    }
}

fn read_chain_head_for_mutation(
    cas_root: &Path,
    refs_root: &Path,
    chain_lock: Option<&ChainLock>,
    chain_root_id: &str,
    trust_store: &TrustStore,
    head_cache: &mut HeadCache,
) -> anyhow::Result<ChainState> {
    let signed_ref = match chain_lock {
        Some(chain_lock) => {
            ensure_lock_protects(chain_lock, chain_root_id)?;
            chain_lock.read_verified_head(trust_store)?
        }
        None => read_current_signed_chain_head(refs_root, chain_root_id, trust_store)?,
    }
    .ok_or_else(|| anyhow::anyhow!("signed ref not found for chain {chain_root_id}"))?;
    let chain_state: ChainState =
        read_cas_object(cas_root, chain_lock, &signed_ref.target_hash, "chain head")?;
    chain_state.validate()?;
    validation::validate_chain_positions(&chain_state)?;
    if chain_state.chain_root_id != chain_root_id {
        anyhow::bail!(
            "chain head object root mismatch: expected {chain_root_id}, got {}",
            chain_state.chain_root_id
        );
    }
    let canonical = lillux::canonical_json(&chain_state.to_value())
        .context("failed to canonicalize current chain head")?;
    let canonical_hash = lillux::sha256_hex(canonical.as_bytes());
    if canonical_hash != signed_ref.target_hash {
        anyhow::bail!(
            "chain head object is not canonically encoded: signed {}, canonical {canonical_hash}",
            signed_ref.target_hash
        );
    }
    head_cache.insert(
        chain_root_id.to_string(),
        CachedHead::new(signed_ref.target_hash, chain_state.clone()),
    );
    Ok(chain_state)
}

fn read_current_snapshot_for_mutation(
    cas_root: &Path,
    chain_lock: Option<&ChainLock>,
    chain_state: &ChainState,
    thread_id: &str,
) -> anyhow::Result<ThreadSnapshot> {
    let entry = chain_state.threads.get(thread_id).ok_or_else(|| {
        anyhow!(
            "thread {thread_id} not found in chain {}",
            chain_state.chain_root_id
        )
    })?;
    let snapshot: ThreadSnapshot = read_cas_object(
        cas_root,
        chain_lock,
        &entry.snapshot_hash,
        &format!("current snapshot for thread {thread_id}"),
    )?;
    let canonical = lillux::canonical_json(&snapshot.to_value())
        .context("failed to canonicalize current thread snapshot")?;
    let canonical_hash = lillux::sha256_hex(canonical.as_bytes());
    if canonical_hash != entry.snapshot_hash {
        anyhow::bail!(
            "current snapshot for thread {thread_id} is not canonically encoded: expected {}, canonical {canonical_hash}",
            entry.snapshot_hash
        );
    }
    validation::validate_stored_snapshot(chain_state, thread_id, entry, &snapshot)
        .with_context(|| format!("invalid current snapshot for thread {thread_id}"))?;
    validate_snapshot_last_event_object(cas_root, chain_lock, &snapshot)?;
    Ok(snapshot)
}

fn validate_snapshot_last_event_object(
    cas_root: &Path,
    chain_lock: Option<&ChainLock>,
    snapshot: &ThreadSnapshot,
) -> anyhow::Result<()> {
    let Some(event_hash) = snapshot.last_event_hash.as_deref() else {
        return Ok(());
    };
    let event: ThreadEvent =
        read_cas_object(cas_root, chain_lock, event_hash, "snapshot last event")?;
    let canonical = lillux::canonical_json(&event.to_value())
        .context("failed to canonicalize snapshot last event")?;
    let canonical_hash = lillux::sha256_hex(canonical.as_bytes());
    if canonical_hash != event_hash {
        anyhow::bail!(
            "snapshot last event is not canonically encoded: expected {event_hash}, canonical {canonical_hash}"
        );
    }
    validation::validate_snapshot_last_event(snapshot, event_hash, &event)
}

pub(crate) fn prospective_mutation_timestamp_floor(
    chain_updated_at: &str,
) -> anyhow::Result<String> {
    validation::monotonic_now([chain_updated_at])
}

pub(crate) fn normalize_prospective_new_thread(
    snapshot: &mut ThreadSnapshot,
    chain_root_id: &str,
    timestamp_floor: &str,
) -> anyhow::Result<()> {
    validation::normalize_and_validate_new_thread(snapshot, chain_root_id, timestamp_floor)
}

pub(crate) fn normalize_prospective_snapshot_transition(
    previous: &ThreadSnapshot,
    next: &mut ThreadSnapshot,
    timestamp_floor: &str,
) -> anyhow::Result<()> {
    validation::normalize_and_validate_snapshot_transition(previous, next, timestamp_floor)
}

/// Read a snapshot through a trust-verified current head.
///
/// A cached chain state is reusable only when its verified content hash is the
/// target of the signed head observed by this read. Cache misses use the same
/// raw-hash, canonical-encoding, structural, and positional validation as the
/// mutation path. Snapshot and last-event objects are always reloaded and
/// validated from CAS before being returned.
pub(crate) fn read_thread_snapshot_with_trust(
    cas_root: &Path,
    refs_root: &Path,
    chain_lock: &ChainLock,
    chain_root_id: &str,
    thread_id: &str,
    trust_store: &TrustStore,
    head_cache: &mut HeadCache,
) -> anyhow::Result<Option<ThreadSnapshot>> {
    validate_chain_root_id(chain_root_id)?;
    chain_lock.ensure_protects(refs_root, chain_root_id)?;
    let Some(signed_ref) = chain_lock.read_verified_head(trust_store)? else {
        head_cache.invalidate(chain_root_id);
        return Ok(None);
    };

    let cached_chain_state = head_cache
        .get(chain_root_id)
        .filter(|cached| cached.chain_state_hash == signed_ref.target_hash)
        .map(|cached| cached.chain_state.clone());
    let chain_state = match cached_chain_state {
        Some(chain_state) => {
            chain_state.validate()?;
            validation::validate_chain_positions(&chain_state)?;
            if chain_state.chain_root_id != chain_root_id {
                anyhow::bail!(
                    "cached chain head object root mismatch: expected {chain_root_id}, got {}",
                    chain_state.chain_root_id
                );
            }
            let canonical = lillux::canonical_json(&chain_state.to_value())
                .context("failed to canonicalize cached chain head")?;
            let canonical_hash = lillux::sha256_hex(canonical.as_bytes());
            if canonical_hash != signed_ref.target_hash {
                anyhow::bail!(
                    "cached chain head hash mismatch: signed {}, canonical {canonical_hash}",
                    signed_ref.target_hash
                );
            }
            chain_state
        }
        _ => read_chain_head_for_mutation(
            cas_root,
            refs_root,
            Some(chain_lock),
            chain_root_id,
            trust_store,
            head_cache,
        )?,
    };

    if !chain_state.threads.contains_key(thread_id) {
        return Ok(None);
    }
    read_current_snapshot_for_mutation(cas_root, Some(chain_lock), &chain_state, thread_id)
        .map(Some)
}

/// Publish a chain head through the durable projection-recovery journal.
///
/// The caller must hold [`ChainLock`] and must keep the daemon/maintenance GC
/// exclusion permit from before its first CAS write until this function
/// returns. The pending Set is durable before the signed head becomes visible.
// The authority-bearing lock, expected head, signer, and trust store stay
// explicit so publication provenance cannot be hidden in an opaque argument bag.
#[allow(clippy::too_many_arguments)]
fn publish_chain_head(
    cas_root: &Path,
    refs_root: &Path,
    chain_lock: &ChainLock,
    chain_root_id: &str,
    expected_previous_hash: Option<&str>,
    target_hash: &str,
    updated_at: &str,
    signer: &dyn Signer,
    trust_store: &TrustStore,
) -> anyhow::Result<()> {
    chain_lock.ensure_protects(refs_root, chain_root_id)?;
    verify_expected_current_head(
        chain_lock,
        chain_root_id,
        expected_previous_hash,
        trust_store,
    )?;
    let _: serde_json::Value =
        read_cas_object(cas_root, Some(chain_lock), target_hash, "new chain head")?;
    let path_recovery;
    let recovery = match chain_lock.recovery_store() {
        Some(recovery) => recovery,
        None => {
            path_recovery = crate::recovery::RecoveryStore::from_refs_root(refs_root)?;
            &path_recovery
        }
    };
    let pending = recovery
        .prepare_set(
            chain_lock,
            chain_root_id,
            expected_previous_hash,
            target_hash,
        )
        .context("failed to prepare durable chain-head Set")?;

    let signed_ref = SignedRef::new(
        format!("chains/{chain_root_id}/head"),
        target_hash.to_string(),
        updated_at.to_string(),
        signer.fingerprint().to_string(),
    );
    chain_lock
        .write_signed_head(signed_ref, signer)
        .context("failed to publish signed chain head")?;
    if pending.phase == crate::recovery::TransitionPhase::Prepared {
        recovery.advance_phase(
            chain_lock,
            chain_root_id,
            &pending.transition_id,
            crate::recovery::TransitionPhase::Prepared,
            crate::recovery::TransitionPhase::HeadPublished,
        )?;
    }
    Ok(())
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
#[cfg(test)]
pub(crate) fn create_chain(
    cas_root: &Path,
    refs_root: &Path,
    chain_root_id: &str,
    initial_snapshot: ThreadSnapshot,
    signer: &dyn Signer,
    trust_store: &TrustStore,
    head_cache: &mut HeadCache,
) -> anyhow::Result<CreateResult> {
    create_chain_with_trust(
        cas_root,
        refs_root,
        chain_root_id,
        initial_snapshot,
        signer,
        trust_store,
        head_cache,
    )
}

#[cfg(test)]
pub(crate) fn create_chain_with_trust(
    cas_root: &Path,
    refs_root: &Path,
    chain_root_id: &str,
    initial_snapshot: ThreadSnapshot,
    signer: &dyn Signer,
    trust_store: &TrustStore,
    head_cache: &mut HeadCache,
) -> anyhow::Result<CreateResult> {
    let _cas_mutation_guard = crate::recovery::CasMutationGuard::shared_from_cas_root(cas_root)?;
    let lock = ChainLock::acquire(refs_root, chain_root_id)?;
    create_chain_with_trust_under_lock(
        cas_root,
        refs_root,
        chain_root_id,
        initial_snapshot,
        signer,
        trust_store,
        head_cache,
        &lock,
    )
}

// Mutation authority and trust inputs deliberately remain explicit at this
// boundary; callers must prove each prerequisite independently.
#[allow(clippy::too_many_arguments)]
pub(crate) fn create_chain_with_trust_under_lock(
    cas_root: &Path,
    refs_root: &Path,
    chain_root_id: &str,
    mut initial_snapshot: ThreadSnapshot,
    signer: &dyn Signer,
    trust_store: &TrustStore,
    head_cache: &mut HeadCache,
    lock: &ChainLock,
) -> anyhow::Result<CreateResult> {
    ensure_lock_protects(lock, chain_root_id)?;
    crate::signer::ensure_signer_trusted(signer, trust_store)?;

    if initial_snapshot.thread_id != chain_root_id
        || initial_snapshot.chain_root_id != chain_root_id
    {
        anyhow::bail!(
            "root thread snapshot must have thread_id and chain_root_id equal to {chain_root_id}"
        );
    }
    let initial_entry = ChainThreadEntry {
        snapshot_hash: String::new(),
        last_event_hash: None,
        last_thread_seq: 0,
        status: initial_snapshot.status,
    };
    validation::stamp_snapshot_position(&mut initial_snapshot, &initial_entry, 0);
    let initial_floor = initial_snapshot.created_at.clone();
    validation::normalize_and_validate_new_thread(
        &mut initial_snapshot,
        chain_root_id,
        &initial_floor,
    )?;
    lock.ensure_protects(refs_root, chain_root_id)?;
    verify_expected_current_head(lock, chain_root_id, None, trust_store)?;

    // Serialize the complete prospective closure before the first CAS write.
    let snapshot_value = initial_snapshot.to_value();
    let snapshot_json = lillux::canonical_json(&snapshot_value)
        .context("failed to canonicalize initial snapshot")?;
    let snapshot_hash = lillux::sha256_hex(snapshot_json.as_bytes());
    let snapshot_path = lillux::shard_path(cas_root, "objects", &snapshot_hash, ".json");

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
        updated_at: validation::monotonic_now([initial_snapshot.updated_at.as_str()])?,
        threads,
    };

    // Validate and store the prospective snapshot and head object together.
    chain_state.validate()?;
    validation::validate_chain_positions(&chain_state)?;
    let chain_state_value = chain_state.to_value();
    let chain_state_json = lillux::canonical_json(&chain_state_value)
        .context("failed to canonicalize initial chain state")?;
    let chain_state_hash = lillux::sha256_hex(chain_state_json.as_bytes());
    let chain_state_path = lillux::shard_path(cas_root, "objects", &chain_state_hash, ".json");
    write_cas_batch(
        cas_root,
        lock,
        &[
            (snapshot_path, snapshot_json.into_bytes()),
            (chain_state_path, chain_state_json.into_bytes()),
        ],
    )
    .context("failed to store initial chain closure in CAS")?;

    publish_chain_head(
        cas_root,
        refs_root,
        lock,
        chain_root_id,
        None,
        &chain_state_hash,
        &chain_state.updated_at,
        signer,
        trust_store,
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
        snapshot: initial_snapshot,
        events: vec![],
    })
}

/// Create a root thread and its initial durable events under one signed head.
///
/// The caller owns the pinned state authority, CAS-mutation guard, and chain
/// lock. Snapshot metadata, event linkage, CAS publication, and the recovery
/// journal therefore form the same indivisible mutation used by every other
/// authoritative chain write.
// Mutation authority and trust inputs deliberately remain explicit at this
// boundary; callers must prove each prerequisite independently.
#[allow(clippy::too_many_arguments)]
pub(crate) fn create_chain_with_events_and_trust_under_lock(
    cas_root: &Path,
    refs_root: &Path,
    chain_root_id: &str,
    mut initial_snapshot: ThreadSnapshot,
    mut events: Vec<ThreadEvent>,
    signer: &dyn Signer,
    trust_store: &TrustStore,
    head_cache: &mut HeadCache,
    lock: &ChainLock,
) -> anyhow::Result<AddThreadWithEventsResult> {
    ensure_lock_protects(lock, chain_root_id)?;
    crate::signer::ensure_signer_trusted(signer, trust_store)?;
    if initial_snapshot.thread_id != chain_root_id
        || initial_snapshot.chain_root_id != chain_root_id
    {
        anyhow::bail!(
            "root thread snapshot must have thread_id and chain_root_id equal to {chain_root_id}"
        );
    }
    if events.is_empty() {
        anyhow::bail!("cannot create root chain with empty event list");
    }
    for event in &events {
        event.validate()?;
        if event.chain_root_id != chain_root_id || event.thread_id != chain_root_id {
            anyhow::bail!("initial root event identity must equal chain_root_id");
        }
    }
    lock.ensure_protects(refs_root, chain_root_id)?;
    verify_expected_current_head(lock, chain_root_id, None, trust_store)?;

    let initial_floor = initial_snapshot.created_at.clone();
    let append_timestamp = validation::normalize_event_timestamps(&mut events, &initial_floor)?;
    let event_count_u64 = u64::try_from(events.len()).context("event batch is too large")?;
    let mut last_event_hash = None;
    let mut stored_events = Vec::with_capacity(events.len());
    let mut pending_writes = Vec::new();
    for (offset, mut event) in events.into_iter().enumerate() {
        let sequence = u64::try_from(offset)
            .context("event batch is too large")?
            .checked_add(1)
            .ok_or_else(|| anyhow!("chain sequence exhausted"))?;
        event.chain_seq = sequence;
        event.thread_seq = sequence;
        event.prev_chain_event_hash = last_event_hash.clone();
        event.prev_thread_event_hash = last_event_hash.clone();
        event.validate()?;

        let event_json = lillux::canonical_json(&event.to_value())
            .context("failed to canonicalize initial root event")?;
        let event_hash = lillux::sha256_hex(event_json.as_bytes());
        let event_path = lillux::shard_path(cas_root, "objects", &event_hash, ".json");
        pending_writes.push((event_path, event_json.into_bytes()));
        last_event_hash = Some(event_hash);
        stored_events.push(event);
    }

    let mut root_entry = ChainThreadEntry {
        snapshot_hash: String::new(),
        last_event_hash: last_event_hash.clone(),
        last_thread_seq: event_count_u64,
        status: initial_snapshot.status,
    };
    validation::stamp_snapshot_position(&mut initial_snapshot, &root_entry, event_count_u64);
    validation::normalize_and_validate_new_thread(
        &mut initial_snapshot,
        chain_root_id,
        &append_timestamp,
    )?;
    validation::validate_snapshot_last_event(
        &initial_snapshot,
        initial_snapshot
            .last_event_hash
            .as_deref()
            .ok_or_else(|| anyhow!("root snapshot is missing its final event hash"))?,
        stored_events
            .last()
            .expect("root creation rejects an empty event list"),
    )?;

    let snapshot_json = lillux::canonical_json(&initial_snapshot.to_value())
        .context("failed to canonicalize initial root snapshot")?;
    let snapshot_hash = lillux::sha256_hex(snapshot_json.as_bytes());
    let snapshot_path = lillux::shard_path(cas_root, "objects", &snapshot_hash, ".json");
    pending_writes.push((snapshot_path, snapshot_json.into_bytes()));
    root_entry.snapshot_hash.clone_from(&snapshot_hash);

    let mut threads = BTreeMap::new();
    threads.insert(chain_root_id.to_string(), root_entry);
    let chain_state = ChainState {
        schema: 1,
        kind: "chain_state".to_string(),
        chain_root_id: chain_root_id.to_string(),
        prev_chain_state_hash: None,
        last_event_hash,
        last_chain_seq: event_count_u64,
        updated_at: validation::monotonic_now([
            append_timestamp.as_str(),
            initial_snapshot.updated_at.as_str(),
        ])?,
        threads,
    };
    chain_state.validate()?;
    validation::validate_chain_positions(&chain_state)?;
    let chain_state_json = lillux::canonical_json(&chain_state.to_value())
        .context("failed to canonicalize initial root chain state")?;
    let chain_state_hash = lillux::sha256_hex(chain_state_json.as_bytes());
    let chain_state_path = lillux::shard_path(cas_root, "objects", &chain_state_hash, ".json");
    pending_writes.push((chain_state_path, chain_state_json.into_bytes()));

    write_cas_batch(cas_root, lock, &pending_writes)
        .context("failed to store initial root chain closure in CAS")?;
    publish_chain_head(
        cas_root,
        refs_root,
        lock,
        chain_root_id,
        None,
        &chain_state_hash,
        &chain_state.updated_at,
        signer,
        trust_store,
    )?;
    head_cache.insert(
        chain_root_id.to_string(),
        CachedHead::new(chain_state_hash.clone(), chain_state),
    );

    Ok(AddThreadWithEventsResult {
        chain_state_hash,
        snapshot_hash,
        snapshot: initial_snapshot,
        first_chain_seq: 1,
        event_count: stored_events.len(),
        events: stored_events,
        snapshot_updates: Vec::new(),
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
#[cfg(test)]
pub(crate) fn append_events(
    input: AppendEventsInput<'_>,
    trust_store: &TrustStore,
    head_cache: &mut HeadCache,
) -> anyhow::Result<AppendResult> {
    append_events_with_trust(input, trust_store, head_cache)
}

#[cfg(test)]
pub(crate) fn append_events_with_trust(
    input: AppendEventsInput<'_>,
    trust_store: &TrustStore,
    head_cache: &mut HeadCache,
) -> anyhow::Result<AppendResult> {
    let _cas_mutation_guard =
        crate::recovery::CasMutationGuard::shared_from_cas_root(input.cas_root)?;
    let lock = ChainLock::acquire(input.refs_root, input.chain_root_id)?;
    append_events_with_trust_under_lock(input, trust_store, head_cache, &lock)
}

pub(crate) fn append_events_with_trust_under_lock(
    input: AppendEventsInput<'_>,
    trust_store: &TrustStore,
    head_cache: &mut HeadCache,
    lock: &ChainLock,
) -> anyhow::Result<AppendResult> {
    let AppendEventsInput {
        cas_root,
        refs_root,
        chain_root_id,
        thread_id,
        mut events,
        mut snapshot_updates,
        signer,
    } = input;

    if events.is_empty() {
        anyhow::bail!("cannot append empty event list");
    }
    ensure_lock_protects(lock, chain_root_id)?;
    crate::signer::ensure_signer_trusted(signer, trust_store)?;

    // Read current chain head
    let current_chain_state = read_chain_head_for_mutation(
        cas_root,
        refs_root,
        Some(lock),
        chain_root_id,
        trust_store,
        head_cache,
    )
    .context("failed to read current chain head")?;

    // Verify thread exists in chain
    if !current_chain_state.threads.contains_key(thread_id) {
        anyhow::bail!("thread {} not found in chain {}", thread_id, chain_root_id);
    }

    let current_thread_entry = &current_chain_state.threads[thread_id];

    // Validate every snapshot update and load its authoritative predecessor
    // before serializing or writing any prospective event object.
    let mut previous_snapshots = BTreeMap::new();
    for update in &snapshot_updates {
        validation::validate_update_identity(
            &update.thread_id,
            &update.new_snapshot,
            chain_root_id,
        )?;
        if previous_snapshots.contains_key(&update.thread_id) {
            anyhow::bail!(
                "duplicate snapshot update for thread {} in one append",
                update.thread_id
            );
        }
        let previous = read_current_snapshot_for_mutation(
            cas_root,
            Some(lock),
            &current_chain_state,
            &update.thread_id,
        )?;
        validation::validate_snapshot_transition_identity(&previous, &update.new_snapshot)?;
        previous_snapshots.insert(update.thread_id.clone(), previous);
    }

    // Validate event identity before assigning authoritative linkage.
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
    let append_timestamp =
        validation::normalize_event_timestamps(&mut events, &current_chain_state.updated_at)?;

    let event_count_u64 = u64::try_from(events.len()).context("event batch is too large")?;
    let first_chain_seq_result = current_chain_state
        .last_chain_seq
        .checked_add(1)
        .ok_or_else(|| anyhow!("chain sequence exhausted"))?;
    let final_chain_seq = current_chain_state
        .last_chain_seq
        .checked_add(event_count_u64)
        .ok_or_else(|| anyhow!("chain sequence exhausted"))?;
    let final_thread_seq = current_thread_entry
        .last_thread_seq
        .checked_add(event_count_u64)
        .ok_or_else(|| anyhow!("thread {thread_id} sequence exhausted"))?;

    // Assign, hash, and stage every event in memory. Nothing is written until
    // the complete prospective snapshot/chain closure has validated.
    let mut stored_events = Vec::new();
    let mut last_event_hash: Option<String> = current_chain_state.last_event_hash.clone();
    let mut last_thread_event_hash: Option<String> = current_thread_entry.last_event_hash.clone();

    let mut pending_writes: Vec<(std::path::PathBuf, Vec<u8>)> = Vec::new();
    for (offset, mut event) in events.into_iter().enumerate() {
        let offset = u64::try_from(offset).context("event batch is too large")?;
        event.chain_seq = first_chain_seq_result
            .checked_add(offset)
            .ok_or_else(|| anyhow!("chain sequence exhausted"))?;
        event.thread_seq = current_thread_entry
            .last_thread_seq
            .checked_add(offset)
            .and_then(|sequence| sequence.checked_add(1))
            .ok_or_else(|| anyhow!("thread {thread_id} sequence exhausted"))?;
        event.prev_chain_event_hash = last_event_hash.clone();
        event.prev_thread_event_hash = last_thread_event_hash.clone();
        event.validate()?;

        let event_value = event.to_value();
        let event_json = lillux::canonical_json(&event_value)
            .context("failed to canonicalize appended event")?;
        let event_hash = lillux::sha256_hex(event_json.as_bytes());
        let event_path = lillux::shard_path(cas_root, "objects", &event_hash, ".json");
        pending_writes.push((event_path, event_json.into_bytes()));

        last_event_hash = Some(event_hash.clone());
        last_thread_event_hash = Some(event_hash);
        stored_events.push(event);
    }
    let event_count = stored_events.len();

    // Establish the exact prospective entry positions before stamping updated
    // snapshots. Callers deliberately pass placeholder linkage because content
    // hashes and allocated sequence numbers are owned by this layer.
    let mut new_threads = current_chain_state.threads.clone();
    let appending_entry = new_threads
        .get_mut(thread_id)
        .expect("appending thread was checked above");
    appending_entry.last_event_hash = last_thread_event_hash;
    appending_entry.last_thread_seq = final_thread_seq;

    for update in &mut snapshot_updates {
        let prospective_entry = new_threads
            .get(&update.thread_id)
            .cloned()
            .expect("snapshot predecessor was checked above");
        validation::stamp_snapshot_position(
            &mut update.new_snapshot,
            &prospective_entry,
            final_chain_seq,
        );
        let previous = previous_snapshots
            .remove(&update.thread_id)
            .expect("snapshot predecessor was loaded above");
        validation::normalize_and_validate_snapshot_transition(
            &previous,
            &mut update.new_snapshot,
            &append_timestamp,
        )?;
        if update.thread_id == thread_id {
            let expected_hash = update
                .new_snapshot
                .last_event_hash
                .as_deref()
                .ok_or_else(|| anyhow!("appending snapshot is missing its final event hash"))?;
            validation::validate_snapshot_last_event(
                &update.new_snapshot,
                expected_hash,
                stored_events
                    .last()
                    .expect("append_events rejects an empty event list"),
            )?;
        } else {
            validate_snapshot_last_event_object(cas_root, Some(lock), &update.new_snapshot)?;
        }

        let snapshot_value = update.new_snapshot.to_value();
        let snapshot_json = lillux::canonical_json(&snapshot_value)
            .context("failed to canonicalize updated snapshot")?;
        let snapshot_hash = lillux::sha256_hex(snapshot_json.as_bytes());
        let snapshot_path = lillux::shard_path(cas_root, "objects", &snapshot_hash, ".json");
        pending_writes.push((snapshot_path, snapshot_json.into_bytes()));
        let thread_entry = new_threads.get_mut(&update.thread_id).ok_or_else(|| {
            anyhow!(
                "attempted to update snapshot for unknown thread {}",
                update.thread_id
            )
        })?;

        thread_entry.snapshot_hash = snapshot_hash;
        thread_entry.status = update.new_snapshot.status;
    }

    let mut changed_threads = snapshot_updates
        .iter()
        .map(|update| update.thread_id.clone())
        .collect::<Vec<_>>();
    changed_threads.push(thread_id.to_string());
    validation::validate_untouched_entries(
        &current_chain_state.threads,
        &new_threads,
        changed_threads,
    )?;

    let mut timestamp_floors = vec![
        current_chain_state.updated_at.as_str(),
        append_timestamp.as_str(),
    ];
    timestamp_floors.extend(
        snapshot_updates
            .iter()
            .map(|update| update.new_snapshot.updated_at.as_str()),
    );

    let current_chain_state_json = lillux::canonical_json(&current_chain_state.to_value())
        .context("failed to canonicalize current chain state")?;
    let new_chain_state = ChainState {
        schema: 1,
        kind: "chain_state".to_string(),
        chain_root_id: chain_root_id.to_string(),
        prev_chain_state_hash: Some(lillux::sha256_hex(current_chain_state_json.as_bytes())),
        last_event_hash,
        last_chain_seq: final_chain_seq,
        updated_at: validation::monotonic_now(timestamp_floors)?,
        threads: new_threads,
    };

    new_chain_state.validate()?;
    validation::validate_chain_positions(&new_chain_state)?;
    let chain_state_value = new_chain_state.to_value();
    let chain_state_json = lillux::canonical_json(&chain_state_value)
        .context("failed to canonicalize new chain state")?;
    let chain_state_hash = lillux::sha256_hex(chain_state_json.as_bytes());
    let chain_state_path = lillux::shard_path(cas_root, "objects", &chain_state_hash, ".json");
    pending_writes.push((chain_state_path, chain_state_json.into_bytes()));

    // One durability barrier for the complete batch. Partial objects after a
    // crash remain unreachable because the signed head is published later.
    write_cas_batch(cas_root, lock, &pending_writes)
        .context("failed to store prospective chain closure in CAS")?;

    publish_chain_head(
        cas_root,
        refs_root,
        lock,
        chain_root_id,
        new_chain_state.prev_chain_state_hash.as_deref(),
        &chain_state_hash,
        &new_chain_state.updated_at,
        signer,
        trust_store,
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
        snapshot_updates,
    })
}

/// Add a new thread to an existing chain.
///
/// Stores the thread's initial snapshot in CAS and updates the chain_state
/// to include the new thread entry. Used when spawning child threads.
#[cfg(test)]
pub(crate) fn add_thread_to_chain(
    cas_root: &Path,
    refs_root: &Path,
    chain_root_id: &str,
    new_snapshot: ThreadSnapshot,
    signer: &dyn Signer,
    trust_store: &TrustStore,
    head_cache: &mut HeadCache,
) -> anyhow::Result<CreateResult> {
    add_thread_to_chain_with_trust(
        cas_root,
        refs_root,
        chain_root_id,
        new_snapshot,
        signer,
        trust_store,
        head_cache,
    )
}

#[cfg(test)]
pub(crate) fn add_thread_to_chain_with_trust(
    cas_root: &Path,
    refs_root: &Path,
    chain_root_id: &str,
    new_snapshot: ThreadSnapshot,
    signer: &dyn Signer,
    trust_store: &TrustStore,
    head_cache: &mut HeadCache,
) -> anyhow::Result<CreateResult> {
    let _cas_mutation_guard = crate::recovery::CasMutationGuard::shared_from_cas_root(cas_root)?;
    let lock = ChainLock::acquire(refs_root, chain_root_id)?;
    add_thread_to_chain_with_trust_under_lock(
        cas_root,
        refs_root,
        chain_root_id,
        new_snapshot,
        signer,
        trust_store,
        head_cache,
        &lock,
    )
}

// Mutation authority and trust inputs deliberately remain explicit at this
// boundary; callers must prove each prerequisite independently.
#[allow(clippy::too_many_arguments)]
pub(crate) fn add_thread_to_chain_with_trust_under_lock(
    cas_root: &Path,
    refs_root: &Path,
    chain_root_id: &str,
    mut new_snapshot: ThreadSnapshot,
    signer: &dyn Signer,
    trust_store: &TrustStore,
    head_cache: &mut HeadCache,
    lock: &ChainLock,
) -> anyhow::Result<CreateResult> {
    ensure_lock_protects(lock, chain_root_id)?;
    crate::signer::ensure_signer_trusted(signer, trust_store)?;
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

    // Read current chain head
    let current_chain_state = read_chain_head_for_mutation(
        cas_root,
        refs_root,
        Some(lock),
        chain_root_id,
        trust_store,
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

    let new_entry_position = ChainThreadEntry {
        snapshot_hash: String::new(),
        last_event_hash: None,
        last_thread_seq: 0,
        status: new_snapshot.status,
    };
    validation::stamp_snapshot_position(
        &mut new_snapshot,
        &new_entry_position,
        current_chain_state.last_chain_seq,
    );
    validation::normalize_and_validate_new_thread(
        &mut new_snapshot,
        chain_root_id,
        &current_chain_state.updated_at,
    )?;

    // Serialize the complete prospective closure before the first CAS write.
    let snapshot_value = new_snapshot.to_value();
    let snapshot_json = lillux::canonical_json(&snapshot_value)
        .context("failed to canonicalize branch snapshot")?;
    let snapshot_hash = lillux::sha256_hex(snapshot_json.as_bytes());
    let snapshot_path = lillux::shard_path(cas_root, "objects", &snapshot_hash, ".json");

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

    let current_chain_state_json = lillux::canonical_json(&current_chain_state.to_value())
        .context("failed to canonicalize current chain state")?;
    let prev_hash = lillux::sha256_hex(current_chain_state_json.as_bytes());

    let new_chain_state = ChainState {
        schema: 1,
        kind: "chain_state".to_string(),
        chain_root_id: chain_root_id.to_string(),
        prev_chain_state_hash: Some(prev_hash),
        last_event_hash: current_chain_state.last_event_hash.clone(),
        last_chain_seq: current_chain_state.last_chain_seq,
        updated_at: validation::monotonic_now([
            current_chain_state.updated_at.as_str(),
            new_snapshot.updated_at.as_str(),
        ])?,
        threads: new_threads,
    };

    new_chain_state.validate()?;
    validation::validate_chain_positions(&new_chain_state)?;
    let chain_state_value = new_chain_state.to_value();
    let chain_state_json = lillux::canonical_json(&chain_state_value)
        .context("failed to canonicalize updated chain state")?;
    let chain_state_hash = lillux::sha256_hex(chain_state_json.as_bytes());
    let chain_state_path = lillux::shard_path(cas_root, "objects", &chain_state_hash, ".json");
    write_cas_batch(
        cas_root,
        lock,
        &[
            (snapshot_path, snapshot_json.into_bytes()),
            (chain_state_path, chain_state_json.into_bytes()),
        ],
    )
    .context("failed to store updated chain closure in CAS")?;

    publish_chain_head(
        cas_root,
        refs_root,
        lock,
        chain_root_id,
        new_chain_state.prev_chain_state_hash.as_deref(),
        &chain_state_hash,
        &new_chain_state.updated_at,
        signer,
        trust_store,
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
        snapshot: new_snapshot,
        events: vec![],
    })
}

#[cfg(test)]
// The test adapter mirrors the authority-bearing production boundary, keeping
// CAS, refs, signer, trust, and cache inputs independently visible.
#[allow(clippy::too_many_arguments)]
pub(crate) fn add_thread_to_chain_with_events(
    cas_root: &Path,
    refs_root: &Path,
    chain_root_id: &str,
    new_snapshot: ThreadSnapshot,
    events: Vec<ThreadEvent>,
    signer: &dyn Signer,
    trust_store: &TrustStore,
    head_cache: &mut HeadCache,
) -> anyhow::Result<AddThreadWithEventsResult> {
    add_thread_to_chain_with_events_and_trust(
        cas_root,
        refs_root,
        chain_root_id,
        new_snapshot,
        events,
        signer,
        trust_store,
        head_cache,
    )
}

#[cfg(test)]
// Preserve the explicit authority/trust seam in tests rather than hiding
// security-relevant inputs inside a convenience fixture.
#[allow(clippy::too_many_arguments)]
pub(crate) fn add_thread_to_chain_with_events_and_trust(
    cas_root: &Path,
    refs_root: &Path,
    chain_root_id: &str,
    new_snapshot: ThreadSnapshot,
    events: Vec<ThreadEvent>,
    signer: &dyn Signer,
    trust_store: &TrustStore,
    head_cache: &mut HeadCache,
) -> anyhow::Result<AddThreadWithEventsResult> {
    let _cas_mutation_guard = crate::recovery::CasMutationGuard::shared_from_cas_root(cas_root)?;
    let lock = ChainLock::acquire(refs_root, chain_root_id)?;
    add_thread_to_chain_with_events_and_trust_under_lock(
        cas_root,
        refs_root,
        chain_root_id,
        new_snapshot,
        events,
        signer,
        trust_store,
        head_cache,
        &lock,
    )
}

// Mutation authority and trust inputs deliberately remain explicit at this
// boundary; callers must prove each prerequisite independently.
#[allow(clippy::too_many_arguments)]
pub(crate) fn add_thread_to_chain_with_events_and_trust_under_lock(
    cas_root: &Path,
    refs_root: &Path,
    chain_root_id: &str,
    mut new_snapshot: ThreadSnapshot,
    mut events: Vec<ThreadEvent>,
    signer: &dyn Signer,
    trust_store: &TrustStore,
    head_cache: &mut HeadCache,
    lock: &ChainLock,
) -> anyhow::Result<AddThreadWithEventsResult> {
    ensure_lock_protects(lock, chain_root_id)?;
    crate::signer::ensure_signer_trusted(signer, trust_store)?;
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

    let current_chain_state = read_chain_head_for_mutation(
        cas_root,
        refs_root,
        Some(lock),
        chain_root_id,
        trust_store,
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

    let append_timestamp =
        validation::normalize_event_timestamps(&mut events, &current_chain_state.updated_at)?;
    let event_count_u64 = u64::try_from(events.len()).context("event batch is too large")?;
    let first_chain_seq = current_chain_state
        .last_chain_seq
        .checked_add(1)
        .ok_or_else(|| anyhow!("chain sequence exhausted"))?;
    let final_chain_seq = current_chain_state
        .last_chain_seq
        .checked_add(event_count_u64)
        .ok_or_else(|| anyhow!("chain sequence exhausted"))?;
    let mut last_event_hash = current_chain_state.last_event_hash.clone();
    let mut last_thread_event_hash: Option<String> = None;
    let mut stored_events = Vec::with_capacity(events.len());
    let mut pending_writes: Vec<(std::path::PathBuf, Vec<u8>)> = Vec::new();

    for (offset, mut event) in events.into_iter().enumerate() {
        let offset = u64::try_from(offset).context("event batch is too large")?;
        event.chain_seq = first_chain_seq
            .checked_add(offset)
            .ok_or_else(|| anyhow!("chain sequence exhausted"))?;
        event.thread_seq = offset
            .checked_add(1)
            .ok_or_else(|| anyhow!("thread sequence exhausted"))?;
        event.prev_chain_event_hash = last_event_hash.clone();
        event.prev_thread_event_hash = last_thread_event_hash.clone();
        event.validate()?;

        let event_value = event.to_value();
        let event_json = lillux::canonical_json(&event_value)
            .context("failed to canonicalize new-thread event")?;
        let event_hash = lillux::sha256_hex(event_json.as_bytes());
        let event_path = lillux::shard_path(cas_root, "objects", &event_hash, ".json");
        pending_writes.push((event_path, event_json.into_bytes()));

        last_event_hash = Some(event_hash.clone());
        last_thread_event_hash = Some(event_hash);
        stored_events.push(event);
    }

    let event_count = stored_events.len();
    let last_thread_seq = event_count_u64;

    let mut new_entry = ChainThreadEntry {
        snapshot_hash: String::new(),
        last_event_hash: last_thread_event_hash,
        last_thread_seq,
        status: new_snapshot.status,
    };
    validation::stamp_snapshot_position(&mut new_snapshot, &new_entry, final_chain_seq);
    validation::normalize_and_validate_new_thread(
        &mut new_snapshot,
        chain_root_id,
        &append_timestamp,
    )?;
    validation::validate_snapshot_last_event(
        &new_snapshot,
        new_snapshot
            .last_event_hash
            .as_deref()
            .ok_or_else(|| anyhow!("new thread snapshot is missing its final event hash"))?,
        stored_events
            .last()
            .expect("add_thread_with_events rejects an empty event list"),
    )?;

    let snapshot_value = new_snapshot.to_value();
    let snapshot_json = lillux::canonical_json(&snapshot_value)
        .context("failed to canonicalize branch snapshot")?;
    let snapshot_hash = lillux::sha256_hex(snapshot_json.as_bytes());
    let snapshot_path = lillux::shard_path(cas_root, "objects", &snapshot_hash, ".json");
    pending_writes.push((snapshot_path, snapshot_json.into_bytes()));
    new_entry.snapshot_hash.clone_from(&snapshot_hash);

    let mut new_threads = current_chain_state.threads.clone();
    new_threads.insert(new_snapshot.thread_id.clone(), new_entry);

    let current_chain_state_json = lillux::canonical_json(&current_chain_state.to_value())
        .context("failed to canonicalize current chain state")?;
    let prev_hash = lillux::sha256_hex(current_chain_state_json.as_bytes());
    let new_chain_state = ChainState {
        schema: 1,
        kind: "chain_state".to_string(),
        chain_root_id: chain_root_id.to_string(),
        prev_chain_state_hash: Some(prev_hash),
        last_event_hash,
        last_chain_seq: final_chain_seq,
        updated_at: validation::monotonic_now([
            current_chain_state.updated_at.as_str(),
            append_timestamp.as_str(),
            new_snapshot.updated_at.as_str(),
        ])?,
        threads: new_threads,
    };

    new_chain_state.validate()?;
    validation::validate_chain_positions(&new_chain_state)?;
    let chain_state_value = new_chain_state.to_value();
    let chain_state_json = lillux::canonical_json(&chain_state_value)
        .context("failed to canonicalize updated chain state")?;
    let chain_state_hash = lillux::sha256_hex(chain_state_json.as_bytes());
    let chain_state_path = lillux::shard_path(cas_root, "objects", &chain_state_hash, ".json");
    pending_writes.push((chain_state_path, chain_state_json.into_bytes()));
    write_cas_batch(cas_root, lock, &pending_writes)
        .context("failed to store branch thread objects in CAS")?;

    publish_chain_head(
        cas_root,
        refs_root,
        lock,
        chain_root_id,
        new_chain_state.prev_chain_state_hash.as_deref(),
        &chain_state_hash,
        &new_chain_state.updated_at,
        signer,
        trust_store,
    )?;

    head_cache.insert(
        chain_root_id.to_string(),
        crate::CachedHead::new(chain_state_hash.clone(), new_chain_state),
    );

    Ok(AddThreadWithEventsResult {
        chain_state_hash,
        snapshot_hash,
        snapshot: new_snapshot,
        first_chain_seq,
        event_count,
        events: stored_events,
        snapshot_updates: Vec::new(),
    })
}

/// Add one thread and append events to an existing thread in one signed head
/// transition. This is the generic cross-thread primitive for continuation
/// creation: neither thread's relation event can become authoritative alone.
#[allow(clippy::too_many_arguments)]
pub(crate) fn add_thread_to_chain_with_events_and_append_with_trust_under_lock(
    cas_root: &Path,
    refs_root: &Path,
    chain_root_id: &str,
    mut new_snapshot: ThreadSnapshot,
    mut new_thread_events: Vec<ThreadEvent>,
    existing_thread_id: &str,
    mut existing_thread_events: Vec<ThreadEvent>,
    mut snapshot_updates: Vec<SnapshotUpdate>,
    signer: &dyn Signer,
    trust_store: &TrustStore,
    head_cache: &mut HeadCache,
    lock: &ChainLock,
) -> anyhow::Result<AddThreadWithEventsResult> {
    ensure_lock_protects(lock, chain_root_id)?;
    crate::signer::ensure_signer_trusted(signer, trust_store)?;
    if new_snapshot.chain_root_id != chain_root_id {
        anyhow::bail!(
            "snapshot chain_root_id mismatch: expected {chain_root_id}, got {}",
            new_snapshot.chain_root_id
        );
    }
    if new_snapshot.thread_id == chain_root_id {
        anyhow::bail!("use root creation for root threads");
    }
    if new_thread_events.is_empty() || existing_thread_events.is_empty() {
        anyhow::bail!("atomic add-and-append requires events for both threads");
    }
    for event in &new_thread_events {
        event.validate()?;
        if event.chain_root_id != chain_root_id || event.thread_id != new_snapshot.thread_id {
            anyhow::bail!("new-thread event identity does not match its snapshot");
        }
    }
    for event in &existing_thread_events {
        event.validate()?;
        if event.chain_root_id != chain_root_id || event.thread_id != existing_thread_id {
            anyhow::bail!("existing-thread event identity does not match its target");
        }
    }

    let current_chain_state = read_chain_head_for_mutation(
        cas_root,
        refs_root,
        Some(lock),
        chain_root_id,
        trust_store,
        head_cache,
    )
    .context("failed to read current chain head")?;
    if current_chain_state
        .threads
        .contains_key(&new_snapshot.thread_id)
    {
        anyhow::bail!(
            "thread {} already exists in chain {chain_root_id}",
            new_snapshot.thread_id
        );
    }
    let existing_entry = current_chain_state
        .threads
        .get(existing_thread_id)
        .cloned()
        .ok_or_else(|| anyhow!("thread {existing_thread_id} not found in chain {chain_root_id}"))?;

    let mut previous_snapshots = BTreeMap::new();
    for update in &snapshot_updates {
        validation::validate_update_identity(
            &update.thread_id,
            &update.new_snapshot,
            chain_root_id,
        )?;
        if update.thread_id == new_snapshot.thread_id {
            anyhow::bail!("new thread snapshot must be supplied through the add operation");
        }
        if previous_snapshots.contains_key(&update.thread_id) {
            anyhow::bail!(
                "duplicate snapshot update for thread {} in one transition",
                update.thread_id
            );
        }
        let previous = read_current_snapshot_for_mutation(
            cas_root,
            Some(lock),
            &current_chain_state,
            &update.thread_id,
        )?;
        validation::validate_snapshot_transition_identity(&previous, &update.new_snapshot)?;
        previous_snapshots.insert(update.thread_id.clone(), previous);
    }

    let new_thread_timestamp = validation::normalize_event_timestamps(
        &mut new_thread_events,
        &current_chain_state.updated_at,
    )?;
    let append_timestamp =
        validation::normalize_event_timestamps(&mut existing_thread_events, &new_thread_timestamp)?;
    let new_event_count =
        u64::try_from(new_thread_events.len()).context("new-thread event batch is too large")?;
    let existing_event_count = u64::try_from(existing_thread_events.len())
        .context("existing-thread event batch is too large")?;
    let total_event_count = new_event_count
        .checked_add(existing_event_count)
        .ok_or_else(|| anyhow!("event batch is too large"))?;
    let first_chain_seq = current_chain_state
        .last_chain_seq
        .checked_add(1)
        .ok_or_else(|| anyhow!("chain sequence exhausted"))?;
    let final_chain_seq = current_chain_state
        .last_chain_seq
        .checked_add(total_event_count)
        .ok_or_else(|| anyhow!("chain sequence exhausted"))?;
    let existing_final_thread_seq = existing_entry
        .last_thread_seq
        .checked_add(existing_event_count)
        .ok_or_else(|| anyhow!("thread {existing_thread_id} sequence exhausted"))?;

    let mut pending_writes = Vec::new();
    let mut stored_events =
        Vec::with_capacity(new_thread_events.len() + existing_thread_events.len());
    let mut last_chain_event_hash = current_chain_state.last_event_hash.clone();
    let mut new_thread_last_hash = None;
    for (offset, mut event) in new_thread_events.into_iter().enumerate() {
        let offset = u64::try_from(offset).context("new-thread event batch is too large")?;
        event.chain_seq = first_chain_seq
            .checked_add(offset)
            .ok_or_else(|| anyhow!("chain sequence exhausted"))?;
        event.thread_seq = offset
            .checked_add(1)
            .ok_or_else(|| anyhow!("new-thread sequence exhausted"))?;
        event.prev_chain_event_hash = last_chain_event_hash.clone();
        event.prev_thread_event_hash = new_thread_last_hash.clone();
        event.validate()?;
        let event_json = lillux::canonical_json(&event.to_value())
            .context("failed to canonicalize new-thread event")?;
        let event_hash = lillux::sha256_hex(event_json.as_bytes());
        let event_path = lillux::shard_path(cas_root, "objects", &event_hash, ".json");
        pending_writes.push((event_path, event_json.into_bytes()));
        last_chain_event_hash = Some(event_hash.clone());
        new_thread_last_hash = Some(event_hash);
        stored_events.push(event);
    }

    let mut existing_thread_last_hash = existing_entry.last_event_hash.clone();
    for (offset, mut event) in existing_thread_events.into_iter().enumerate() {
        let offset = u64::try_from(offset).context("existing-thread event batch is too large")?;
        event.chain_seq = first_chain_seq
            .checked_add(new_event_count)
            .and_then(|sequence| sequence.checked_add(offset))
            .ok_or_else(|| anyhow!("chain sequence exhausted"))?;
        event.thread_seq = existing_entry
            .last_thread_seq
            .checked_add(offset)
            .and_then(|sequence| sequence.checked_add(1))
            .ok_or_else(|| anyhow!("thread {existing_thread_id} sequence exhausted"))?;
        event.prev_chain_event_hash = last_chain_event_hash.clone();
        event.prev_thread_event_hash = existing_thread_last_hash.clone();
        event.validate()?;
        let event_json = lillux::canonical_json(&event.to_value())
            .context("failed to canonicalize existing-thread event")?;
        let event_hash = lillux::sha256_hex(event_json.as_bytes());
        let event_path = lillux::shard_path(cas_root, "objects", &event_hash, ".json");
        pending_writes.push((event_path, event_json.into_bytes()));
        last_chain_event_hash = Some(event_hash.clone());
        existing_thread_last_hash = Some(event_hash);
        stored_events.push(event);
    }

    let mut new_threads = current_chain_state.threads.clone();
    let mut new_entry = ChainThreadEntry {
        snapshot_hash: String::new(),
        last_event_hash: new_thread_last_hash,
        last_thread_seq: new_event_count,
        status: new_snapshot.status,
    };
    validation::stamp_snapshot_position(&mut new_snapshot, &new_entry, final_chain_seq);
    validation::normalize_and_validate_new_thread(
        &mut new_snapshot,
        chain_root_id,
        &append_timestamp,
    )?;
    let new_thread_last_event_index =
        usize::try_from(new_event_count - 1).context("new-thread event batch is too large")?;
    validation::validate_snapshot_last_event(
        &new_snapshot,
        new_snapshot
            .last_event_hash
            .as_deref()
            .ok_or_else(|| anyhow!("new thread snapshot is missing its final event hash"))?,
        &stored_events[new_thread_last_event_index],
    )?;
    let snapshot_json = lillux::canonical_json(&new_snapshot.to_value())
        .context("failed to canonicalize new thread snapshot")?;
    let snapshot_hash = lillux::sha256_hex(snapshot_json.as_bytes());
    let snapshot_path = lillux::shard_path(cas_root, "objects", &snapshot_hash, ".json");
    pending_writes.push((snapshot_path, snapshot_json.into_bytes()));
    new_entry.snapshot_hash.clone_from(&snapshot_hash);
    new_threads.insert(new_snapshot.thread_id.clone(), new_entry);

    let existing = new_threads
        .get_mut(existing_thread_id)
        .expect("existing thread was checked above");
    existing.last_event_hash = existing_thread_last_hash;
    existing.last_thread_seq = existing_final_thread_seq;

    for update in &mut snapshot_updates {
        let prospective_entry = new_threads
            .get(&update.thread_id)
            .cloned()
            .expect("snapshot predecessor was checked above");
        validation::stamp_snapshot_position(
            &mut update.new_snapshot,
            &prospective_entry,
            final_chain_seq,
        );
        let previous = previous_snapshots
            .remove(&update.thread_id)
            .expect("snapshot predecessor was loaded above");
        validation::normalize_and_validate_snapshot_transition(
            &previous,
            &mut update.new_snapshot,
            &append_timestamp,
        )?;
        if update.thread_id == existing_thread_id {
            let expected_hash =
                update
                    .new_snapshot
                    .last_event_hash
                    .as_deref()
                    .ok_or_else(|| {
                        anyhow!("existing thread snapshot is missing its final event hash")
                    })?;
            validation::validate_snapshot_last_event(
                &update.new_snapshot,
                expected_hash,
                stored_events
                    .last()
                    .expect("atomic add-and-append requires existing-thread events"),
            )?;
        } else {
            validate_snapshot_last_event_object(cas_root, Some(lock), &update.new_snapshot)?;
        }
        let update_json = lillux::canonical_json(&update.new_snapshot.to_value())
            .context("failed to canonicalize updated thread snapshot")?;
        let update_hash = lillux::sha256_hex(update_json.as_bytes());
        let update_path = lillux::shard_path(cas_root, "objects", &update_hash, ".json");
        pending_writes.push((update_path, update_json.into_bytes()));
        let entry = new_threads
            .get_mut(&update.thread_id)
            .expect("snapshot update existence was checked above");
        entry.snapshot_hash = update_hash;
        entry.status = update.new_snapshot.status;
    }

    let mut changed_threads = snapshot_updates
        .iter()
        .map(|update| update.thread_id.clone())
        .collect::<Vec<_>>();
    changed_threads.push(existing_thread_id.to_string());
    validation::validate_untouched_entries(
        &current_chain_state.threads,
        &new_threads,
        changed_threads,
    )?;

    let mut timestamp_floors = vec![
        current_chain_state.updated_at.as_str(),
        append_timestamp.as_str(),
        new_snapshot.updated_at.as_str(),
    ];
    timestamp_floors.extend(
        snapshot_updates
            .iter()
            .map(|update| update.new_snapshot.updated_at.as_str()),
    );
    let current_json = lillux::canonical_json(&current_chain_state.to_value())
        .context("failed to canonicalize current chain state")?;
    let new_chain_state = ChainState {
        schema: 1,
        kind: "chain_state".to_string(),
        chain_root_id: chain_root_id.to_string(),
        prev_chain_state_hash: Some(lillux::sha256_hex(current_json.as_bytes())),
        last_event_hash: last_chain_event_hash,
        last_chain_seq: final_chain_seq,
        updated_at: validation::monotonic_now(timestamp_floors)?,
        threads: new_threads,
    };
    new_chain_state.validate()?;
    validation::validate_chain_positions(&new_chain_state)?;
    let chain_state_json = lillux::canonical_json(&new_chain_state.to_value())
        .context("failed to canonicalize atomic add-and-append chain state")?;
    let chain_state_hash = lillux::sha256_hex(chain_state_json.as_bytes());
    let chain_state_path = lillux::shard_path(cas_root, "objects", &chain_state_hash, ".json");
    pending_writes.push((chain_state_path, chain_state_json.into_bytes()));

    write_cas_batch(cas_root, lock, &pending_writes)
        .context("failed to store atomic add-and-append chain closure in CAS")?;
    publish_chain_head(
        cas_root,
        refs_root,
        lock,
        chain_root_id,
        new_chain_state.prev_chain_state_hash.as_deref(),
        &chain_state_hash,
        &new_chain_state.updated_at,
        signer,
        trust_store,
    )?;
    head_cache.insert(
        chain_root_id.to_string(),
        CachedHead::new(chain_state_hash.clone(), new_chain_state),
    );

    Ok(AddThreadWithEventsResult {
        chain_state_hash,
        snapshot_hash,
        snapshot: new_snapshot,
        first_chain_seq,
        event_count: stored_events.len(),
        events: stored_events,
        snapshot_updates,
    })
}

/// Read the current chain head.
///
/// First tries the in-memory head cache. If not found, reads and verifies
/// the signed ref from the refs directory.
#[cfg(test)]
pub(crate) fn read_chain_head(
    cas_root: &Path,
    refs_root: &Path,
    chain_root_id: &str,
    trust_store: &TrustStore,
    head_cache: &mut HeadCache,
) -> anyhow::Result<ChainState> {
    validate_chain_root_id(chain_root_id)?;
    read_chain_head_for_mutation(
        cas_root,
        refs_root,
        None,
        chain_root_id,
        trust_store,
        head_cache,
    )
}

/// Read a thread's snapshot from a chain.
///
/// Returns None if the thread doesn't exist in the chain.
#[cfg(test)]
pub(crate) fn read_thread_snapshot(
    cas_root: &Path,
    refs_root: &Path,
    chain_root_id: &str,
    thread_id: &str,
    trust_store: &TrustStore,
    head_cache: &mut HeadCache,
) -> anyhow::Result<ReadSnapshotResult> {
    let chain_state = read_chain_head(cas_root, refs_root, chain_root_id, trust_store, head_cache)?;

    let canonical = lillux::canonical_json(&chain_state.to_value())
        .context("failed to canonicalize chain head")?;
    let chain_state_hash = lillux::sha256_hex(canonical.as_bytes());

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

    validation::validate_stored_snapshot(&chain_state, thread_id, thread_entry, &snapshot)?;
    let canonical = lillux::canonical_json(&snapshot.to_value())
        .context("failed to canonicalize thread snapshot")?;
    let actual_snapshot_hash = lillux::sha256_hex(canonical.as_bytes());
    if actual_snapshot_hash != thread_entry.snapshot_hash {
        anyhow::bail!(
            "snapshot CAS hash mismatch for thread {thread_id}: expected {}, got {actual_snapshot_hash}",
            thread_entry.snapshot_hash
        );
    }
    validate_snapshot_last_event_object(cas_root, None, &snapshot)?;

    Ok(ReadSnapshotResult {
        snapshot: Some(snapshot),
        chain_state_hash,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signer::TestSigner;
    use ryeos_tracing::test;

    #[test]
    fn acquire_existing_chain_lock_never_creates_authority_state() {
        let temp = tempfile::tempdir().unwrap();
        let refs_root = temp.path().join("refs");
        assert!(ChainLock::acquire_existing(&refs_root, "T-root").is_err());
        assert!(!refs_root.exists());

        drop(ChainLock::acquire(&refs_root, "T-root").unwrap());
        drop(ChainLock::acquire_existing(&refs_root, "T-root").unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn chain_lock_rejects_symlinked_refs_ancestor() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let outside = temp.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        let refs_root = temp.path().join("refs");
        symlink(&outside, &refs_root).unwrap();

        assert!(ChainLock::acquire(&refs_root, "T-root").is_err());
        assert!(!outside.join("generic").exists());
    }

    fn test_trust_store() -> TrustStore {
        let signer = TestSigner::default();
        let mut trust_store = TrustStore::new();
        trust_store.insert(signer.fingerprint().to_string(), signer.verifying_key());
        trust_store
    }

    fn acknowledge_test_projection(refs_root: &Path, chain_root_id: &str) {
        let recovery = crate::recovery::RecoveryStore::from_refs_root(refs_root).unwrap();
        let lock = ChainLock::acquire(refs_root, chain_root_id).unwrap();
        let mut pending = recovery.read_pending(chain_root_id).unwrap().unwrap();
        if pending.phase == crate::recovery::TransitionPhase::HeadPublished {
            pending = recovery
                .advance_phase(
                    &lock,
                    chain_root_id,
                    &pending.transition_id,
                    crate::recovery::TransitionPhase::HeadPublished,
                    crate::recovery::TransitionPhase::ApplyingProjection,
                )
                .unwrap();
        }
        recovery
            .acknowledge(&lock, chain_root_id, &pending.transition_id)
            .unwrap();
    }

    fn create_chain(
        cas_root: &Path,
        refs_root: &Path,
        chain_root_id: &str,
        initial_snapshot: ThreadSnapshot,
        signer: &dyn Signer,
        head_cache: &mut HeadCache,
    ) -> anyhow::Result<CreateResult> {
        let trust_store = test_trust_store();
        let result = super::create_chain(
            cas_root,
            refs_root,
            chain_root_id,
            initial_snapshot,
            signer,
            &trust_store,
            head_cache,
        );
        if result.is_ok() {
            acknowledge_test_projection(refs_root, chain_root_id);
        }
        result
    }

    fn append_events(
        input: AppendEventsInput<'_>,
        head_cache: &mut HeadCache,
    ) -> anyhow::Result<AppendResult> {
        let refs_root = input.refs_root.to_path_buf();
        let chain_root_id = input.chain_root_id.to_string();
        let trust_store = test_trust_store();
        let result = super::append_events(input, &trust_store, head_cache);
        if result.is_ok() {
            acknowledge_test_projection(&refs_root, &chain_root_id);
        }
        result
    }

    fn add_thread_to_chain(
        cas_root: &Path,
        refs_root: &Path,
        chain_root_id: &str,
        snapshot: ThreadSnapshot,
        signer: &dyn Signer,
        head_cache: &mut HeadCache,
    ) -> anyhow::Result<CreateResult> {
        let trust_store = test_trust_store();
        let result = super::add_thread_to_chain(
            cas_root,
            refs_root,
            chain_root_id,
            snapshot,
            signer,
            &trust_store,
            head_cache,
        );
        if result.is_ok() {
            acknowledge_test_projection(refs_root, chain_root_id);
        }
        result
    }

    fn add_thread_to_chain_with_events(
        cas_root: &Path,
        refs_root: &Path,
        chain_root_id: &str,
        snapshot: ThreadSnapshot,
        events: Vec<ThreadEvent>,
        signer: &dyn Signer,
        head_cache: &mut HeadCache,
    ) -> anyhow::Result<AddThreadWithEventsResult> {
        let trust_store = test_trust_store();
        let result = super::add_thread_to_chain_with_events(
            cas_root,
            refs_root,
            chain_root_id,
            snapshot,
            events,
            signer,
            &trust_store,
            head_cache,
        );
        if result.is_ok() {
            acknowledge_test_projection(refs_root, chain_root_id);
        }
        result
    }

    fn read_chain_head(
        cas_root: &Path,
        refs_root: &Path,
        chain_root_id: &str,
        head_cache: &mut HeadCache,
    ) -> anyhow::Result<ChainState> {
        super::read_chain_head(
            cas_root,
            refs_root,
            chain_root_id,
            &test_trust_store(),
            head_cache,
        )
    }

    fn read_thread_snapshot(
        cas_root: &Path,
        refs_root: &Path,
        chain_root_id: &str,
        thread_id: &str,
        head_cache: &mut HeadCache,
    ) -> anyhow::Result<ReadSnapshotResult> {
        super::read_thread_snapshot(
            cas_root,
            refs_root,
            chain_root_id,
            thread_id,
            &test_trust_store(),
            head_cache,
        )
    }

    fn make_test_snapshot(thread_id: &str) -> ThreadSnapshot {
        use crate::objects::thread_snapshot::{
            CapturedItemTrustClass, CapturedNodeHistoryPolicyProvenance, CapturedPolicyProvenance,
            CapturedThreadHistoryPolicy, ThreadHistoryRetention, ThreadSnapshotBuilder,
        };

        let hash = "11".repeat(32);

        ThreadSnapshotBuilder::new(
            thread_id,
            thread_id,
            "directive",
            "system/test",
            "directive-runtime",
        )
        .captured_history_policy(Some(CapturedThreadHistoryPolicy {
            retention: ThreadHistoryRetention::Durable,
            canonical_item_ref: "system/test".into(),
            item_content_hash: hash.clone(),
            item_signer_fingerprint: Some(hash.clone()),
            item_trust_class: CapturedItemTrustClass::Trusted,
            kind_schema_content_hash: hash,
            resolved_from: CapturedPolicyProvenance::NodeDefault {
                node_policy: CapturedNodeHistoryPolicyProvenance::MissingConfig,
            },
        }))
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

        let signed_ref = refs::read_signed_ref_envelope_structural(&ref_path).unwrap();
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
    fn append_stamps_snapshot_with_authoritative_event_position() {
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

        let mut running =
            read_thread_snapshot(&cas_root, &refs_root, "T-root", "T-root", &mut head_cache)
                .unwrap()
                .snapshot
                .unwrap();
        running.status = crate::objects::ThreadStatus::Running;
        running.started_at = Some(running.created_at.clone());
        // Callers do not know the allocated sequence or content hash. Deliberate
        // placeholders prove the chain layer owns and stamps this linkage.
        running.last_event_hash = None;
        running.last_chain_seq = 0;
        running.last_thread_seq = 0;

        let result = append_events(
            AppendEventsInput {
                cas_root: &cas_root,
                refs_root: &refs_root,
                chain_root_id: "T-root",
                thread_id: "T-root",
                events: vec![make_test_event("T-root", "T-root")],
                snapshot_updates: vec![SnapshotUpdate {
                    thread_id: "T-root".into(),
                    new_snapshot: running,
                }],
                signer: &signer,
            },
            &mut head_cache,
        )
        .unwrap();

        let stored =
            read_thread_snapshot(&cas_root, &refs_root, "T-root", "T-root", &mut head_cache)
                .unwrap()
                .snapshot
                .unwrap();
        let event_hash = lillux::sha256_hex(
            lillux::canonical_json(&result.events[0].to_value())
                .unwrap()
                .as_bytes(),
        );
        assert_eq!(stored.status, crate::objects::ThreadStatus::Running);
        assert_eq!(stored.last_event_hash.as_deref(), Some(event_hash.as_str()));
        assert_eq!(stored.last_chain_seq, 1);
        assert_eq!(stored.last_thread_seq, 1);
    }

    #[test]
    fn append_rejects_mismatched_snapshot_update_before_writing_cas() {
        fn count_files(path: &Path) -> usize {
            let Ok(entries) = std::fs::read_dir(path) else {
                return 0;
            };
            entries
                .filter_map(Result::ok)
                .map(|entry| {
                    if entry.path().is_dir() {
                        count_files(&entry.path())
                    } else {
                        1
                    }
                })
                .sum()
        }

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

        let snapshot =
            read_thread_snapshot(&cas_root, &refs_root, "T-root", "T-root", &mut head_cache)
                .unwrap()
                .snapshot
                .unwrap();
        let before = count_files(&cas_root);
        let error = append_events(
            AppendEventsInput {
                cas_root: &cas_root,
                refs_root: &refs_root,
                chain_root_id: "T-root",
                thread_id: "T-root",
                events: vec![make_test_event("T-root", "T-root")],
                snapshot_updates: vec![SnapshotUpdate {
                    thread_id: "T-other".into(),
                    new_snapshot: snapshot,
                }],
                signer: &signer,
            },
            &mut head_cache,
        )
        .unwrap_err();

        assert!(error
            .to_string()
            .contains("does not match snapshot thread_id"));
        assert_eq!(count_files(&cas_root), before);
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
                lillux::canonical_json(&result.events[0].to_value())
                    .unwrap()
                    .as_bytes()
            ))
        );

        let head = read_chain_head(&cas_root, &refs_root, "T-root", &mut head_cache).unwrap();
        let child = head.threads.get("T-child").expect("child thread entry");
        assert_eq!(head.last_chain_seq, 2);
        assert_eq!(child.last_thread_seq, 2);
        assert_eq!(child.last_event_hash, head.last_event_hash);

        let child_snapshot =
            read_thread_snapshot(&cas_root, &refs_root, "T-root", "T-child", &mut head_cache)
                .unwrap()
                .snapshot
                .unwrap();
        assert_eq!(child_snapshot.last_chain_seq, 2);
        assert_eq!(child_snapshot.last_thread_seq, 2);
        assert_eq!(child_snapshot.last_event_hash, child.last_event_hash);
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
        let head = read_chain_head(&cas_root, &refs_root, "T-root", &mut head_cache).unwrap();
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
        let result =
            read_thread_snapshot(&cas_root, &refs_root, "T-root", "T-root", &mut head_cache)
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
            &mut head_cache,
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
