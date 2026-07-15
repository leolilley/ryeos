//! Projection rebuild and catch-up from CAS.
//!
//! The projection (SQLite) is a rebuildable view of immutable CAS objects.
//! If projection.sqlite3 is deleted or corrupted, everything can be
//! recovered by walking CAS from signed heads.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};
use crate::projection::{project_event, project_thread_snapshot, ProjectionDb};
use crate::objects::{ChainThreadEntry, EventDurability};
use crate::{ChainState, ThreadEvent, ThreadSnapshot};

#[cfg(not(test))]
const REBUILD_TX_CHAIN_BATCH: usize = 200;
#[cfg(test)]
const REBUILD_TX_CHAIN_BATCH: usize = 2;

/// Report from a full projection rebuild.
#[derive(Debug, Clone, Default)]
pub struct RebuildReport {
    pub chains_rebuilt: usize,
    pub threads_restored: usize,
    pub events_projected: usize,
}

/// Report from an incremental catch-up.
#[derive(Debug, Clone, Default)]
pub struct CatchUpReport {
    pub chains_checked: usize,
    pub chains_updated: usize,
    pub threads_restored: usize,
    pub events_projected: usize,
}

/// Time-based progress reporter for the rebuild/catch-up loops. These run
/// synchronously on the daemon boot path and can grind for minutes on a big
/// store; without a heartbeat the boot is indistinguishable from a hang from
/// the outside (the control socket does not exist yet).
struct RebuildProgress {
    started: std::time::Instant,
    next_note: std::time::Instant,
}

const REBUILD_PROGRESS_EVERY: std::time::Duration = std::time::Duration::from_secs(15);

impl RebuildProgress {
    fn new() -> Self {
        let now = std::time::Instant::now();
        Self {
            started: now,
            next_note: now + REBUILD_PROGRESS_EVERY,
        }
    }

    fn note(&mut self, stage: &str, chains: usize, threads: usize, events: usize) {
        if std::time::Instant::now() < self.next_note {
            return;
        }
        tracing::info!(
            stage,
            chains,
            threads,
            events,
            elapsed_s = self.started.elapsed().as_secs(),
            "projection {stage} in progress"
        );
        self.next_note = std::time::Instant::now() + REBUILD_PROGRESS_EVERY;
    }
}

/// Full rebuild: delete and recreate projection from CAS.
///
/// Walks every signed chain head and projects all thread snapshots
/// and durable events into the projection database.
#[tracing::instrument(
    name = "state:rebuild",
    skip(projection, cas_root, refs_root, trust_store)
)]
pub fn rebuild_projection(
    projection: &ProjectionDb,
    cas_root: &Path,
    refs_root: &Path,
    trust_store: &crate::refs::TrustStore,
) -> Result<RebuildReport> {
    let mut report = RebuildReport::default();
    let mut progress = RebuildProgress::new();

    // Clear existing projection tables (schema will be re-created)
    let conn = projection.connection();
    conn.execute_batch(
        "DELETE FROM projection_meta;
         DELETE FROM threads;
         DELETE FROM events;
         DELETE FROM event_replay_index;
         DELETE FROM thread_edges;
         DELETE FROM thread_results;
         DELETE FROM thread_artifacts;
         DELETE FROM thread_facets;
         DELETE FROM thread_usage_latest;
         DELETE FROM thread_usage_subjects;",
    )
    .context("failed to clear projection tables")?;

    let entries = chain_ref_entries(refs_root)?;
    if entries.is_empty() {
        return Ok(report);
    }

    let mut batch = ProjectionBatch::new(projection, "rebuild");

    for entry in entries {
        let chain_root_id = entry.file_name().to_string_lossy().to_string();
        let head_path = entry.path().join("head");
        if !head_path.exists() {
            continue;
        }

        let verified_head = crate::chain::load_verified_chain_head(
            cas_root,
            refs_root,
            &chain_root_id,
            trust_store,
        )
        .with_context(|| format!("verifying authoritative head for chain {chain_root_id}"))?;
        let chain_state_hash = verified_head.chain_state_hash.as_str();

        batch.ensure_started()?;

        // Walk chain history (newest to oldest via prev_chain_state_hash)
        let chain_report = rebuild_chain(projection, cas_root, &chain_root_id, chain_state_hash)?;

        // Update projection_meta to point to the head
        // Read the head chain_state to get updated_at
        projection.update_projection_meta(&crate::projection::ProjectionMeta {
            chain_root_id: chain_root_id.clone(),
            indexed_chain_state_hash: chain_state_hash.to_owned(),
            updated_at: verified_head.chain_state.updated_at.clone(),
        })?;

        report.chains_rebuilt += 1;
        report.threads_restored += chain_report.threads;
        report.events_projected += chain_report.events;
        batch.note_chain()?;
        progress.note(
            "rebuild",
            report.chains_rebuilt,
            report.threads_restored,
            report.events_projected,
        );
    }

    batch.commit_partial()?;

    Ok(report)
}

/// Incremental catch-up: find chains where projection is behind CAS
/// and project the delta.
#[tracing::instrument(
    name = "state:catch_up",
    skip(projection, cas_root, refs_root, trust_store)
)]
pub fn catch_up_projection(
    projection: &ProjectionDb,
    cas_root: &Path,
    refs_root: &Path,
    trust_store: &crate::refs::TrustStore,
) -> Result<CatchUpReport> {
    let mut report = CatchUpReport::default();
    let mut progress = RebuildProgress::new();

    let entries = chain_ref_entries(refs_root)?;
    if entries.is_empty() {
        return Ok(report);
    }

    let mut batch = ProjectionBatch::new(projection, "catch-up");

    for entry in entries {
        let chain_root_id = entry.file_name().to_string_lossy().to_string();
        let head_path = entry.path().join("head");
        if !head_path.exists() {
            continue;
        }

        report.chains_checked += 1;

        let verified_head = crate::chain::load_verified_chain_head(
            cas_root,
            refs_root,
            &chain_root_id,
            trust_store,
        )
        .with_context(|| format!("verifying authoritative head for chain {chain_root_id}"))?;
        let head_hash = verified_head.chain_state_hash.as_str();

        // Check projection_meta
        let current_meta = projection.get_projection_meta(&chain_root_id)?;
        let indexed_hash = current_meta
            .as_ref()
            .map(|m| m.indexed_chain_state_hash.as_str());

        if indexed_hash == Some(head_hash) {
            // Already up to date
            continue;
        }

        batch.ensure_started()?;

        // Projection is behind. Rebuild from indexed point (or full if no meta).
        let chain_report = if let Some(meta) = current_meta {
            // Incremental: walk from indexed state to head
            rebuild_chain_delta(
                projection,
                cas_root,
                &chain_root_id,
                &meta.indexed_chain_state_hash,
                head_hash,
            )?
        } else {
            // No meta — full chain rebuild
            rebuild_chain(projection, cas_root, &chain_root_id, head_hash)?
        };

        // Update projection_meta
        projection.update_projection_meta(&crate::projection::ProjectionMeta {
            chain_root_id: chain_root_id.clone(),
            indexed_chain_state_hash: head_hash.to_owned(),
            updated_at: verified_head.chain_state.updated_at.clone(),
        })?;

        report.chains_updated += 1;
        report.threads_restored += chain_report.threads;
        report.events_projected += chain_report.events;
        batch.note_chain()?;
        progress.note(
            "catch-up",
            report.chains_updated,
            report.threads_restored,
            report.events_projected,
        );
    }

    batch.commit_partial()?;

    Ok(report)
}

fn chain_ref_entries(refs_root: &Path) -> Result<Vec<std::fs::DirEntry>> {
    let chains_dir = refs_root.join("generic/chains");
    if !chains_dir.is_dir() {
        return Ok(Vec::new());
    }

    let mut entries = Vec::new();
    for entry in std::fs::read_dir(&chains_dir).context("failed to read chains refs directory")? {
        let entry = entry.context("failed to read chain ref entry")?;
        if entry.file_type()?.is_dir() {
            entries.push(entry);
        }
    }
    entries.sort_by_key(|entry| entry.file_name());
    Ok(entries)
}

struct ProjectionBatch<'a> {
    projection: &'a ProjectionDb,
    stage: &'static str,
    tx: Option<rusqlite::Transaction<'a>>,
    chains: usize,
}

impl<'a> ProjectionBatch<'a> {
    fn new(projection: &'a ProjectionDb, stage: &'static str) -> Self {
        Self {
            projection,
            stage,
            tx: None,
            chains: 0,
        }
    }

    fn ensure_started(&mut self) -> Result<()> {
        if self.tx.is_none() {
            // Projection rebuild/catch-up code reachable under this batch must
            // not start its own transaction; rusqlite/SQLite will reject nested
            // BEGINs on this connection. Keep sync-job immediate transactions
            // out of this path.
            self.tx = Some(
                self.projection
                    .connection()
                    .unchecked_transaction()
                    .with_context(|| {
                        format!("failed to begin projection {} transaction", self.stage)
                    })?,
            );
        }
        Ok(())
    }

    fn note_chain(&mut self) -> Result<()> {
        self.chains += 1;
        if self.chains >= REBUILD_TX_CHAIN_BATCH {
            self.commit_partial()?;
        }
        Ok(())
    }

    fn commit_partial(&mut self) -> Result<()> {
        if let Some(tx) = self.tx.take() {
            tx.commit().with_context(|| {
                format!("failed to commit projection {} transaction", self.stage)
            })?;
            self.chains = 0;
        }
        Ok(())
    }
}

/// Internal report for a single chain rebuild.
struct ChainReport {
    threads: usize,
    events: usize,
}

/// Rebuild a single chain's projection by walking chain_state history
/// from the given head hash toward earlier links via prev_chain_state_hash.
///
/// Projects all thread snapshots and durable events found along the way.
fn rebuild_chain(
    projection: &ProjectionDb,
    cas_root: &Path,
    chain_root_id: &str,
    head_hash: &str,
) -> Result<ChainReport> {
    let history = load_chain_history(cas_root, chain_root_id, head_hash)?;
    let head = &history
        .first()
        .expect("verified chain history always contains its head")
        .1;
    let events = verify_event_history(cas_root, head)?;
    verify_chain_history_objects(cas_root, &history, &events)?;
    let threads = project_authoritative_snapshots(projection, cas_root, head, &events)?;
    let events = project_verified_events(projection, &events, None)?;
    Ok(ChainReport { threads, events })
}

/// Rebuild only the delta from `from_hash` to `to_hash` for a chain.
fn rebuild_chain_delta(
    projection: &ProjectionDb,
    cas_root: &Path,
    chain_root_id: &str,
    from_hash: &str,
    to_hash: &str,
) -> Result<ChainReport> {
    let history = load_chain_history(cas_root, chain_root_id, to_hash)?;
    let head = &history
        .first()
        .expect("verified chain history always contains its head")
        .1;
    let Some((_, from_state)) = history.iter().find(|(hash, _)| hash == from_hash) else {
        return rebuild_chain(projection, cas_root, chain_root_id, to_hash);
    };

    let events = verify_event_history(cas_root, head)?;
    verify_chain_history_objects(cas_root, &history, &events)?;
    let threads = project_authoritative_snapshots(projection, cas_root, head, &events)?;
    let events = project_verified_events(projection, &events, Some(from_state))?;
    Ok(ChainReport { threads, events })
}

fn load_chain_state(cas_root: &Path, hash: &str, chain_root_id: &str) -> Result<ChainState> {
    let path = lillux::shard_path(cas_root, "objects", hash, ".json");
    let json = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read chain_state {hash} from CAS"))?;
    let state: ChainState = serde_json::from_str(&json)
        .with_context(|| format!("failed to parse chain_state {hash}"))?;
    state
        .validate()
        .with_context(|| format!("invalid chain_state {hash}"))?;
    let actual_hash = crate::objects::chain_state::hash_chain_state(&state);
    if actual_hash != hash {
        anyhow::bail!("chain_state CAS hash mismatch: named {hash}, object hashes to {actual_hash}");
    }
    if state.chain_root_id != chain_root_id {
        anyhow::bail!(
            "chain_state identity mismatch: expected {chain_root_id}, got {}",
            state.chain_root_id
        );
    }
    Ok(state)
}

fn load_chain_history(
    cas_root: &Path,
    chain_root_id: &str,
    head_hash: &str,
) -> Result<Vec<(String, ChainState)>> {
    let mut history = Vec::new();
    let mut visited = HashSet::new();
    let mut current_hash = head_hash.to_string();
    loop {
        if !visited.insert(current_hash.clone()) {
            anyhow::bail!("cycle in chain_state history at {current_hash}");
        }
        let state = load_chain_state(cas_root, &current_hash, chain_root_id)?;
        if let Some((newer_hash, newer)) = history.last() {
            if state.last_chain_seq > newer.last_chain_seq {
                anyhow::bail!(
                    "chain_state sequence regressed from older {current_hash} ({}) to newer \
                     {newer_hash} ({})",
                    state.last_chain_seq,
                    newer.last_chain_seq
                );
            }
        }
        let previous = state.prev_chain_state_hash.clone();
        history.push((current_hash, state));
        match previous {
            Some(hash) => current_hash = hash,
            None => break,
        }
    }
    Ok(history)
}

fn load_thread_snapshot(
    cas_root: &Path,
    chain: &ChainState,
    thread_id: &str,
    entry: &ChainThreadEntry,
    event_index: &HashMap<String, ThreadEvent>,
) -> Result<ThreadSnapshot> {
    let hash = &entry.snapshot_hash;
    let path = lillux::shard_path(cas_root, "objects", hash, ".json");
    let json = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read thread_snapshot {hash} from CAS"))?;
    let snapshot: ThreadSnapshot = serde_json::from_str(&json)
        .with_context(|| format!("failed to parse thread_snapshot {hash}"))?;
    snapshot
        .validate()
        .with_context(|| format!("invalid thread_snapshot {hash}"))?;
    let actual_hash = lillux::sha256_hex(lillux::canonical_json(&snapshot.to_value()).as_bytes());
    if actual_hash != *hash {
        anyhow::bail!("thread_snapshot CAS hash mismatch: named {hash}, object hashes to {actual_hash}");
    }
    if snapshot.thread_id != thread_id || snapshot.chain_root_id != chain.chain_root_id {
        anyhow::bail!(
            "thread_snapshot identity mismatch for {thread_id}: snapshot thread={}, chain={}",
            snapshot.thread_id,
            snapshot.chain_root_id
        );
    }
    if snapshot.status != entry.status {
        anyhow::bail!(
            "thread_snapshot status mismatch for {thread_id}: entry={}, snapshot={}",
            entry.status,
            snapshot.status
        );
    }
    if snapshot.last_thread_seq > entry.last_thread_seq {
        anyhow::bail!(
            "thread_snapshot last_thread_seq exceeds chain entry for {thread_id}: entry={}, snapshot={}",
            entry.last_thread_seq,
            snapshot.last_thread_seq
        );
    }
    if snapshot.last_chain_seq > chain.last_chain_seq {
        anyhow::bail!(
            "thread_snapshot last_chain_seq {} exceeds chain head {} for {thread_id}",
            snapshot.last_chain_seq,
            chain.last_chain_seq
        );
    }
    match snapshot.last_event_hash.as_deref() {
        None if snapshot.last_thread_seq == 0 && snapshot.last_chain_seq == 0 => {}
        None => anyhow::bail!(
            "thread_snapshot without an event has non-zero cursors for {thread_id}"
        ),
        Some(event_hash) => {
            let event = event_index.get(event_hash).ok_or_else(|| {
                anyhow::anyhow!(
                    "thread_snapshot event {event_hash} is not reachable from chain head"
                )
            })?;
            if event.thread_id != thread_id
                || event.thread_seq != snapshot.last_thread_seq
                || event.chain_seq != snapshot.last_chain_seq
            {
                anyhow::bail!(
                    "thread_snapshot cursor does not identify its declared thread event for {thread_id}"
                );
            }
        }
    }
    if snapshot.last_thread_seq == entry.last_thread_seq
        && snapshot.last_event_hash != entry.last_event_hash
    {
        anyhow::bail!(
            "current thread_snapshot event cursor does not match chain entry for {thread_id}"
        );
    }
    Ok(snapshot)
}

fn project_authoritative_snapshots(
    projection: &ProjectionDb,
    cas_root: &Path,
    head: &ChainState,
    events: &[VerifiedEvent],
) -> Result<usize> {
    let event_index = event_index(events);
    for (thread_id, entry) in &head.threads {
        let snapshot = load_thread_snapshot(cas_root, head, thread_id, entry, &event_index)?;
        project_thread_snapshot(projection, &snapshot, &head.chain_root_id)?;
    }
    Ok(head.threads.len())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ThreadCursor {
    hash: Option<String>,
    seq: u64,
}

fn thread_cursors(state: &ChainState) -> HashMap<String, ThreadCursor> {
    state
        .threads
        .iter()
        .map(|(thread_id, entry)| {
            (
                thread_id.clone(),
                ThreadCursor {
                    hash: entry.last_event_hash.clone(),
                    seq: entry.last_thread_seq,
                },
            )
        })
        .collect()
}

fn load_thread_event(cas_root: &Path, hash: &str, chain_root_id: &str) -> Result<ThreadEvent> {
    let path = lillux::shard_path(cas_root, "objects", hash, ".json");
    let json = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read thread_event {hash} from CAS"))?;
    let event: ThreadEvent = serde_json::from_str(&json)
        .with_context(|| format!("failed to parse thread_event {hash}"))?;
    event
        .validate()
        .with_context(|| format!("invalid thread_event {hash}"))?;
    let actual_hash = crate::objects::thread_event::hash_event(&event);
    if actual_hash != hash {
        anyhow::bail!("thread_event CAS hash mismatch: named {hash}, object hashes to {actual_hash}");
    }
    if event.chain_root_id != chain_root_id {
        anyhow::bail!(
            "thread_event chain mismatch: expected {chain_root_id}, got {}",
            event.chain_root_id
        );
    }
    if !event.durability.is_cas_stored() {
        anyhow::bail!("ephemeral thread_event {hash} is reachable from an authoritative head");
    }
    Ok(event)
}

#[derive(Debug, Clone)]
struct VerifiedEvent {
    hash: String,
    event: ThreadEvent,
}

fn event_index(events: &[VerifiedEvent]) -> HashMap<String, ThreadEvent> {
    events
        .iter()
        .map(|verified| (verified.hash.clone(), verified.event.clone()))
        .collect()
}

/// Verify the complete event closure reachable from an authoritative chain
/// head. Events are returned newest first.
fn verify_event_history(
    cas_root: &Path,
    head: &ChainState,
) -> Result<Vec<VerifiedEvent>> {
    match (&head.last_event_hash, head.last_chain_seq) {
        (None, 0) => {}
        (Some(_), seq) if seq > 0 => {}
        _ => anyhow::bail!("chain head event hash/sequence cursors are contradictory"),
    }

    let mut current_hash = head.last_event_hash.clone();
    let mut expected_chain_seq = head.last_chain_seq;
    let mut cursors = thread_cursors(head);
    let mut visited = HashSet::new();
    let mut events = Vec::new();

    while let Some(hash) = current_hash {
        if !visited.insert(hash.clone()) {
            anyhow::bail!("cycle in thread_event chain at {hash}");
        }
        let event = load_thread_event(cas_root, &hash, &head.chain_root_id)?;
        if event.chain_seq != expected_chain_seq || expected_chain_seq == 0 {
            anyhow::bail!(
                "thread_event chain sequence mismatch at {hash}: expected {expected_chain_seq}, got {}",
                event.chain_seq
            );
        }
        let cursor = cursors.get_mut(&event.thread_id).ok_or_else(|| {
            anyhow::anyhow!("thread_event {hash} names absent thread {}", event.thread_id)
        })?;
        if cursor.hash.as_deref() != Some(hash.as_str()) || cursor.seq != event.thread_seq {
            anyhow::bail!("thread_event per-thread cursor mismatch at {hash}");
        }
        cursor.hash = event.prev_thread_event_hash.clone();
        cursor.seq = cursor.seq.checked_sub(1).ok_or_else(|| {
            anyhow::anyhow!("thread_event thread sequence underflow at {hash}")
        })?;
        current_hash = event.prev_chain_event_hash.clone();
        expected_chain_seq -= 1;
        events.push(VerifiedEvent { hash, event });
    }

    if expected_chain_seq != 0
        || cursors
            .values()
            .any(|cursor| cursor.hash.is_some() || cursor.seq != 0)
    {
        anyhow::bail!("event walk ended before all authoritative cursors reached genesis");
    }
    Ok(events)
}

fn verify_chain_history_objects(
    cas_root: &Path,
    history: &[(String, ChainState)],
    events: &[VerifiedEvent],
) -> Result<()> {
    let event_index = event_index(events);
    for (state_hash, state) in history {
        verify_chain_state_cursors(state_hash, state, &event_index)?;
        for (thread_id, entry) in &state.threads {
            load_thread_snapshot(cas_root, state, thread_id, entry, &event_index).with_context(
                || {
                    format!(
                        "verifying snapshot {} reachable from chain_state {state_hash}",
                        entry.snapshot_hash
                    )
                },
            )?;
        }
    }
    Ok(())
}

fn verify_chain_state_cursors(
    state_hash: &str,
    state: &ChainState,
    events: &HashMap<String, ThreadEvent>,
) -> Result<()> {
    match state.last_event_hash.as_deref() {
        None if state.last_chain_seq == 0 => {}
        None => anyhow::bail!(
            "chain_state {state_hash} has no last event but non-zero chain sequence"
        ),
        Some(hash) => {
            let event = events.get(hash).ok_or_else(|| {
                anyhow::anyhow!(
                    "chain_state {state_hash} event cursor {hash} is not reachable from head"
                )
            })?;
            if event.chain_seq != state.last_chain_seq {
                anyhow::bail!(
                    "chain_state {state_hash} event cursor sequence mismatch: expected {}, got {}",
                    state.last_chain_seq,
                    event.chain_seq
                );
            }
        }
    }

    for (thread_id, entry) in &state.threads {
        match entry.last_event_hash.as_deref() {
            None if entry.last_thread_seq == 0 => {}
            None => anyhow::bail!(
                "chain_state {state_hash} thread {thread_id} has no last event but non-zero sequence"
            ),
            Some(hash) => {
                let event = events.get(hash).ok_or_else(|| {
                    anyhow::anyhow!(
                        "chain_state {state_hash} thread cursor {hash} is not reachable from head"
                    )
                })?;
                if event.thread_id != *thread_id
                    || event.thread_seq != entry.last_thread_seq
                    || event.chain_seq > state.last_chain_seq
                {
                    anyhow::bail!(
                        "chain_state {state_hash} thread cursor does not identify its declared event for {thread_id}"
                    );
                }
            }
        }
    }
    Ok(())
}

/// Project only events after the already-indexed state. The full event history
/// has already been verified, so a missing incremental boundary is a hard
/// error rather than a reason to mark the projection current.
fn project_verified_events(
    projection: &ProjectionDb,
    events: &[VerifiedEvent],
    stop: Option<&ChainState>,
) -> Result<usize> {
    let boundary = stop.and_then(|state| state.last_event_hash.as_deref());
    let take = match boundary {
        Some(hash) => events
            .iter()
            .position(|event| event.hash == hash)
            .ok_or_else(|| {
                anyhow::anyhow!("indexed event boundary {hash} is not reachable from authoritative head")
            })?,
        None => events.len(),
    };

    let mut count = 0;
    for verified in events[..take].iter().rev() {
        if verified.event.durability == EventDurability::Durable {
            project_event(projection, &verified.event)?;
            count += 1;
        }
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signer::TestSigner;
    use crate::Signer;
    use ryeos_tracing::test as trace_test;
    use serde_json::Value;

    fn rebuild_projection(
        projection: &ProjectionDb,
        cas_root: &Path,
        refs_root: &Path,
    ) -> Result<RebuildReport> {
        let signer = TestSigner::default();
        super::rebuild_projection(
            projection,
            cas_root,
            refs_root,
            &crate::signer::trust_store_for_signer(&signer),
        )
    }

    fn catch_up_projection(
        projection: &ProjectionDb,
        cas_root: &Path,
        refs_root: &Path,
    ) -> Result<CatchUpReport> {
        let signer = TestSigner::default();
        super::catch_up_projection(
            projection,
            cas_root,
            refs_root,
            &crate::signer::trust_store_for_signer(&signer),
        )
    }

    fn write_object(cas_root: &Path, value: &Value) -> String {
        let canonical = lillux::canonical_json(value);
        let hash = lillux::sha256_hex(canonical.as_bytes());
        let path = lillux::shard_path(cas_root, "objects", &hash, ".json");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        lillux::atomic_write(&path, canonical.as_bytes()).unwrap();
        hash
    }

    fn write_signed_head(refs_root: &Path, chain_root_id: &str, target_hash: &str) {
        let head_path = refs_root
            .join("generic/chains")
            .join(chain_root_id)
            .join("head");
        let signer = TestSigner::default();
        let signed_ref = crate::SignedRef::new(
            format!("chains/{chain_root_id}/head"),
            target_hash.to_owned(),
            "2026-04-22T00:00:00Z".to_owned(),
            signer.fingerprint().to_owned(),
        );
        crate::refs::write_signed_ref(&head_path, signed_ref, &signer).unwrap();
    }

    fn make_chain_state(
        chain_root_id: &str,
        prev_hash: Option<&str>,
        threads: Vec<(&str, &str, Option<&str>, u64, &str)>,
        last_event_hash: Option<&str>,
        last_chain_seq: u64,
    ) -> Value {
        let mut threads_map = serde_json::Map::new();
        for (tid, snap_hash, evt_hash, thread_seq, status) in threads {
            let entry = serde_json::json!({
                "snapshot_hash": snap_hash,
                "last_event_hash": evt_hash,
                "last_thread_seq": thread_seq,
                "status": status,
            });
            threads_map.insert(tid.to_string(), entry);
        }

        serde_json::json!({
            "kind": "chain_state",
            "schema": 1,
            "chain_root_id": chain_root_id,
            "prev_chain_state_hash": prev_hash,
            "last_event_hash": last_event_hash,
            "last_chain_seq": last_chain_seq,
            "updated_at": "2026-04-22T00:00:00Z",
            "threads": threads_map,
        })
    }

    fn make_snapshot_json(thread_id: &str, chain_root_id: &str, status: &str) -> Value {
        serde_json::json!({
            "kind": "thread_snapshot",
            "schema": 1,
            "thread_id": thread_id,
            "chain_root_id": chain_root_id,
            "status": status,
            "kind_name": "directive",
            "item_ref": "test/item",
            "executor_ref": "test/executor",
            "launch_mode": "inline",
            "current_site_id": "site:test",
            "origin_site_id": "site:test",
            "base_project_snapshot_hash": null,
            "result_project_snapshot_hash": null,
            "created_at": "2026-04-22T00:00:00Z",
            "updated_at": "2026-04-22T00:00:00Z",
            "started_at": if status == "running" { serde_json::json!("2026-04-22T00:00:00Z") } else { serde_json::Value::Null },
            "finished_at": null,
            "result": null,
            "error": null,
            "budget": null,
            "artifacts": [],
            "facets": {},
            "last_event_hash": null,
            "last_chain_seq": 0,
            "last_thread_seq": 0,
            "upstream_thread_id": null,
            "requested_by": null,
        })
    }

    fn set_snapshot_cursor(snapshot: &mut Value, event_hash: &str, chain_seq: u64, thread_seq: u64) {
        snapshot["last_event_hash"] = Value::String(event_hash.to_owned());
        snapshot["last_chain_seq"] = Value::from(chain_seq);
        snapshot["last_thread_seq"] = Value::from(thread_seq);
    }

    fn make_event_json(
        chain_root_id: &str,
        thread_id: &str,
        chain_seq: u64,
        thread_seq: u64,
        prev_chain: Option<&str>,
        event_type: &str,
    ) -> Value {
        serde_json::json!({
            "kind": "thread_event",
            "schema": 1,
            "chain_root_id": chain_root_id,
            "chain_seq": chain_seq,
            "thread_id": thread_id,
            "thread_seq": thread_seq,
            "event_type": event_type,
            "durability": "durable",
            "ts": "2026-04-22T00:00:00Z",
            "prev_chain_event_hash": prev_chain,
            "prev_thread_event_hash": prev_chain,
            "payload": {}
        })
    }

    #[test]
    fn rebuild_projection_single_chain() {
        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("objects");
        let refs_root = tmp.path().join("refs");
        std::fs::create_dir_all(&cas_root).unwrap();
        std::fs::create_dir_all(&refs_root).unwrap();

        let event = make_event_json("T-root", "T-root", 1, 1, None, "thread_created");
        let event_hash = write_object(&cas_root, &event);
        let mut snap = make_snapshot_json("T-root", "T-root", "running");
        set_snapshot_cursor(&mut snap, &event_hash, 1, 1);
        let snap_hash = write_object(&cas_root, &snap);
        let cs = make_chain_state(
            "T-root",
            None,
            vec![("T-root", &snap_hash, Some(&event_hash), 1, "running")],
            Some(&event_hash),
            1,
        );
        let cs_hash = write_object(&cas_root, &cs);
        write_signed_head(&refs_root, "T-root", &cs_hash);

        let proj_path = tmp.path().join("projection.db");
        let proj = ProjectionDb::open(&proj_path).unwrap();

        let report = rebuild_projection(&proj, &cas_root, &refs_root).unwrap();
        assert_eq!(report.chains_rebuilt, 1);
        assert_eq!(report.threads_restored, 1);
        assert_eq!(report.events_projected, 1);

        // Verify projection has the thread
        let conn = proj.connection();
        let status: String = conn
            .query_row(
                "SELECT status FROM threads WHERE thread_id = 'T-root'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "running");

        // Verify projection has the event
        let event_count: usize = conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE thread_id = 'T-root'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(event_count, 1);
    }

    #[test]
    fn rebuild_projection_multiple_chains() {
        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("objects");
        let refs_root = tmp.path().join("refs");
        std::fs::create_dir_all(&cas_root).unwrap();
        std::fs::create_dir_all(&refs_root).unwrap();

        for chain_id in ["T-a", "T-b"] {
            let snap = make_snapshot_json(chain_id, chain_id, "created");
            let snap_hash = write_object(&cas_root, &snap);
            let cs = make_chain_state(
                chain_id,
                None,
                vec![(chain_id, &snap_hash, None, 0, "created")],
                None,
                0,
            );
            let cs_hash = write_object(&cas_root, &cs);
            write_signed_head(&refs_root, chain_id, &cs_hash);
        }

        let proj_path = tmp.path().join("projection.db");
        let proj = ProjectionDb::open(&proj_path).unwrap();

        let report = rebuild_projection(&proj, &cas_root, &refs_root).unwrap();
        assert_eq!(report.chains_rebuilt, 2);
    }

    #[test]
    fn rebuild_projection_chain_history() {
        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("objects");
        let refs_root = tmp.path().join("refs");
        std::fs::create_dir_all(&cas_root).unwrap();
        std::fs::create_dir_all(&refs_root).unwrap();

        let snap1 = make_snapshot_json("T-root", "T-root", "created");
        let snap1_hash = write_object(&cas_root, &snap1);
        let event = make_event_json("T-root", "T-root", 1, 1, None, "thread_started");
        let event_hash = write_object(&cas_root, &event);
        let mut snap2 = make_snapshot_json("T-root", "T-root", "running");
        set_snapshot_cursor(&mut snap2, &event_hash, 1, 1);
        let snap2_hash = write_object(&cas_root, &snap2);

        let cs1 = make_chain_state(
            "T-root",
            None,
            vec![("T-root", &snap1_hash, None, 0, "created")],
            None,
            0,
        );
        let cs1_hash = write_object(&cas_root, &cs1);

        let cs2 = make_chain_state(
            "T-root",
            Some(&cs1_hash),
            vec![("T-root", &snap2_hash, Some(&event_hash), 1, "running")],
            Some(&event_hash),
            1,
        );
        let cs2_hash = write_object(&cas_root, &cs2);
        write_signed_head(&refs_root, "T-root", &cs2_hash);

        let proj_path = tmp.path().join("projection.db");
        let proj = ProjectionDb::open(&proj_path).unwrap();

        let report = rebuild_projection(&proj, &cas_root, &refs_root).unwrap();
        assert_eq!(report.chains_rebuilt, 1);
        // Thread count = 1 (same thread_id, latest snapshot wins)
        assert_eq!(report.threads_restored, 1);
        assert_eq!(report.events_projected, 1);

        // Verify latest status
        let conn = proj.connection();
        let status: String = conn
            .query_row(
                "SELECT status FROM threads WHERE thread_id = 'T-root'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "running");
    }

    #[test]
    fn catch_up_projection_noop_when_current() {
        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("objects");
        let refs_root = tmp.path().join("refs");
        std::fs::create_dir_all(&cas_root).unwrap();
        std::fs::create_dir_all(&refs_root).unwrap();

        let snap = make_snapshot_json("T-root", "T-root", "created");
        let snap_hash = write_object(&cas_root, &snap);
        let cs = make_chain_state(
            "T-root",
            None,
            vec![("T-root", &snap_hash, None, 0, "created")],
            None,
            0,
        );
        let cs_hash = write_object(&cas_root, &cs);
        write_signed_head(&refs_root, "T-root", &cs_hash);

        let proj_path = tmp.path().join("projection.db");
        let proj = ProjectionDb::open(&proj_path).unwrap();

        // Set projection_meta to match current head
        let meta = crate::projection::ProjectionMeta {
            chain_root_id: "T-root".to_string(),
            indexed_chain_state_hash: cs_hash,
            updated_at: "2026-04-22T00:00:00Z".to_string(),
        };
        proj.update_projection_meta(&meta).unwrap();

        let report = catch_up_projection(&proj, &cas_root, &refs_root).unwrap();
        assert_eq!(report.chains_checked, 1);
        assert_eq!(report.chains_updated, 0);
    }

    #[test]
    fn catch_up_projection_updates_when_behind() {
        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("objects");
        let refs_root = tmp.path().join("refs");
        std::fs::create_dir_all(&cas_root).unwrap();
        std::fs::create_dir_all(&refs_root).unwrap();

        let snap1 = make_snapshot_json("T-root", "T-root", "created");
        let snap1_hash = write_object(&cas_root, &snap1);
        let event = make_event_json("T-root", "T-root", 1, 1, None, "thread_started");
        let event_hash = write_object(&cas_root, &event);
        let mut snap2 = make_snapshot_json("T-root", "T-root", "running");
        set_snapshot_cursor(&mut snap2, &event_hash, 1, 1);
        let snap2_hash = write_object(&cas_root, &snap2);

        let cs1 = make_chain_state(
            "T-root",
            None,
            vec![("T-root", &snap1_hash, None, 0, "created")],
            None,
            0,
        );
        let cs1_hash = write_object(&cas_root, &cs1);
        let cs2 = make_chain_state(
            "T-root",
            Some(&cs1_hash),
            vec![("T-root", &snap2_hash, Some(&event_hash), 1, "running")],
            Some(&event_hash),
            1,
        );
        let cs2_hash = write_object(&cas_root, &cs2);
        write_signed_head(&refs_root, "T-root", &cs2_hash);

        let proj_path = tmp.path().join("projection.db");
        let proj = ProjectionDb::open(&proj_path).unwrap();

        // Set projection_meta to cs1 (behind)
        let meta = crate::projection::ProjectionMeta {
            chain_root_id: "T-root".to_string(),
            indexed_chain_state_hash: cs1_hash.clone(),
            updated_at: "2026-04-22T00:00:00Z".to_string(),
        };
        proj.update_projection_meta(&meta).unwrap();

        let report = catch_up_projection(&proj, &cas_root, &refs_root).unwrap();
        assert_eq!(report.chains_checked, 1);
        assert_eq!(report.chains_updated, 1);
        assert_eq!(report.events_projected, 1);

        // Verify latest status
        let conn = proj.connection();
        let status: String = conn
            .query_row(
                "SELECT status FROM threads WHERE thread_id = 'T-root'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "running");
    }

    #[test]
    fn catch_up_delta_with_multiple_states_projects_the_newest_snapshot() {
        // A thread that changes state more than once inside one catch-up
        // delta (running -> completed between two boots) must read back at
        // its NEWEST snapshot. Projecting the delta's earliest snapshot
        // regresses a finished thread to `running`, and the startup
        // reconciler then finalizes it failed against its own record.
        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("objects");
        let refs_root = tmp.path().join("refs");
        std::fs::create_dir_all(&cas_root).unwrap();
        std::fs::create_dir_all(&refs_root).unwrap();

        let snap1 = make_snapshot_json("T-multi", "T-multi", "created");
        let snap2 = make_snapshot_json("T-multi", "T-multi", "running");
        let snap3 = make_snapshot_json("T-multi", "T-multi", "completed");
        let snap1_hash = write_object(&cas_root, &snap1);
        let snap2_hash = write_object(&cas_root, &snap2);
        let snap3_hash = write_object(&cas_root, &snap3);

        let cs1 = make_chain_state(
            "T-multi",
            None,
            vec![("T-multi", &snap1_hash, None, 0, "created")],
            None,
            0,
        );
        let cs1_hash = write_object(&cas_root, &cs1);
        let cs2 = make_chain_state(
            "T-multi",
            Some(&cs1_hash),
            vec![("T-multi", &snap2_hash, None, 0, "running")],
            None,
            0,
        );
        let cs2_hash = write_object(&cas_root, &cs2);
        let cs3 = make_chain_state(
            "T-multi",
            Some(&cs2_hash),
            vec![("T-multi", &snap3_hash, None, 0, "completed")],
            None,
            0,
        );
        let cs3_hash = write_object(&cas_root, &cs3);
        write_signed_head(&refs_root, "T-multi", &cs3_hash);

        let proj_path = tmp.path().join("projection.db");
        let proj = ProjectionDb::open(&proj_path).unwrap();
        let meta = crate::projection::ProjectionMeta {
            chain_root_id: "T-multi".to_string(),
            indexed_chain_state_hash: cs1_hash.clone(),
            updated_at: "2026-04-22T00:00:00Z".to_string(),
        };
        proj.update_projection_meta(&meta).unwrap();

        let report = catch_up_projection(&proj, &cas_root, &refs_root).unwrap();
        assert_eq!(report.chains_updated, 1);

        let conn = proj.connection();
        let status: String = conn
            .query_row(
                "SELECT status FROM threads WHERE thread_id = 'T-multi'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            status, "completed",
            "the delta's newest snapshot must win, never its earliest"
        );
    }

    #[test]
    fn catch_up_projection_batches_more_than_batch_size() {
        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("objects");
        let refs_root = tmp.path().join("refs");
        std::fs::create_dir_all(&cas_root).unwrap();
        std::fs::create_dir_all(&refs_root).unwrap();

        for chain_id in ["T-a", "T-b", "T-c"] {
            let snap = make_snapshot_json(chain_id, chain_id, "created");
            let snap_hash = write_object(&cas_root, &snap);
            let cs = make_chain_state(
                chain_id,
                None,
                vec![(chain_id, &snap_hash, None, 0, "created")],
                None,
                0,
            );
            let cs_hash = write_object(&cas_root, &cs);
            write_signed_head(&refs_root, chain_id, &cs_hash);
        }

        let proj_path = tmp.path().join("projection.db");
        let proj = ProjectionDb::open(&proj_path).unwrap();

        let report = catch_up_projection(&proj, &cas_root, &refs_root).unwrap();
        assert_eq!(report.chains_checked, 3);
        assert_eq!(report.chains_updated, 3);
        assert_eq!(report.threads_restored, 3);

        let conn = proj.connection();
        let thread_count: usize = conn
            .query_row("SELECT COUNT(*) FROM threads", [], |row| row.get(0))
            .unwrap();
        assert_eq!(thread_count, 3);
        let meta_count: usize = conn
            .query_row("SELECT COUNT(*) FROM projection_meta", [], |row| row.get(0))
            .unwrap();
        assert_eq!(meta_count, 3);
    }

    #[test]
    fn catch_up_projection_error_rolls_back_only_current_batch() {
        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("objects");
        let refs_root = tmp.path().join("refs");
        std::fs::create_dir_all(&cas_root).unwrap();
        std::fs::create_dir_all(&refs_root).unwrap();

        for chain_id in ["T-a", "T-b"] {
            let snap = make_snapshot_json(chain_id, chain_id, "created");
            let snap_hash = write_object(&cas_root, &snap);
            let cs = make_chain_state(
                chain_id,
                None,
                vec![(chain_id, &snap_hash, None, 0, "created")],
                None,
                0,
            );
            let cs_hash = write_object(&cas_root, &cs);
            write_signed_head(&refs_root, chain_id, &cs_hash);
        }

        let bad_snap = make_snapshot_json("T-c", "T-c", "created");
        let bad_snap_hash = write_object(&cas_root, &bad_snap);
        let bad_event = make_event_json(
            "T-c",
            "T-c",
            1,
            1,
            Some("not-a-valid-hash"),
            "thread_started",
        );
        let bad_event_hash = write_object(&cas_root, &bad_event);
        let bad_cs = make_chain_state(
            "T-c",
            None,
            vec![("T-c", &bad_snap_hash, Some(&bad_event_hash), 1, "created")],
            Some(&bad_event_hash),
            1,
        );
        let bad_cs_hash = write_object(&cas_root, &bad_cs);
        write_signed_head(&refs_root, "T-c", &bad_cs_hash);

        let proj_path = tmp.path().join("projection.db");
        let proj = ProjectionDb::open(&proj_path).unwrap();

        let err = catch_up_projection(&proj, &cas_root, &refs_root).unwrap_err();
        assert!(
            err.to_string().contains("invalid prev_chain_event_hash"),
            "unexpected error: {err:#}"
        );

        let conn = proj.connection();
        let committed_meta_count: usize = conn
            .query_row("SELECT COUNT(*) FROM projection_meta", [], |row| row.get(0))
            .unwrap();
        assert_eq!(committed_meta_count, 2, "first committed batch remains");
        let rolled_back_meta_count: usize = conn
            .query_row(
                "SELECT COUNT(*) FROM projection_meta WHERE chain_root_id = 'T-c'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(rolled_back_meta_count, 0, "failed batch meta rolls back");
        let rolled_back_thread_count: usize = conn
            .query_row(
                "SELECT COUNT(*) FROM threads WHERE thread_id = 'T-c'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(rolled_back_thread_count, 0, "failed batch rows roll back");
    }

    #[test]
    fn rebuild_projection_empty_no_panic() {
        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("objects");
        let refs_root = tmp.path().join("refs");
        std::fs::create_dir_all(&cas_root).unwrap();
        std::fs::create_dir_all(&refs_root).unwrap();

        let proj_path = tmp.path().join("projection.db");
        let proj = ProjectionDb::open(&proj_path).unwrap();

        let report = rebuild_projection(&proj, &cas_root, &refs_root).unwrap();
        assert_eq!(report.chains_rebuilt, 0);
        assert_eq!(report.threads_restored, 0);
        assert_eq!(report.events_projected, 0);
    }

    // ── Trace-capture tests ──────────────────────────────────────

    #[test]
    fn rebuild_projection_emits_span() {
        let tempdir = tempfile::tempdir().unwrap();
        let cas_root = tempdir.path().join("cas");
        let refs_root = tempdir.path().join("refs");
        let proj_path = tempdir.path().join("projection.db");
        let proj = ProjectionDb::open(&proj_path).unwrap();

        // Empty rebuild — still should emit the span
        let (_, spans) = trace_test::capture_traces(|| {
            let _ = rebuild_projection(&proj, &cas_root, &refs_root);
        });

        let span = trace_test::find_span(&spans, "state:rebuild");
        assert!(
            span.is_some(),
            "expected state:rebuild span, got: {:?}",
            spans
                .iter()
                .map(|s: &ryeos_tracing::test::RecordedSpan| &s.name)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn catch_up_projection_emits_span() {
        let tempdir = tempfile::tempdir().unwrap();
        let cas_root = tempdir.path().join("cas");
        let refs_root = tempdir.path().join("refs");
        let proj_path = tempdir.path().join("projection.db");
        let proj = ProjectionDb::open(&proj_path).unwrap();

        let (_, spans) = trace_test::capture_traces(|| {
            let _ = catch_up_projection(&proj, &cas_root, &refs_root);
        });

        let span = trace_test::find_span(&spans, "state:catch_up");
        assert!(
            span.is_some(),
            "expected state:catch_up span, got: {:?}",
            spans
                .iter()
                .map(|s: &ryeos_tracing::test::RecordedSpan| &s.name)
                .collect::<Vec<_>>()
        );
    }
}
