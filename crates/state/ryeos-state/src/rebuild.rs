//! Projection rebuild and catch-up from CAS.
//!
//! The projection (SQLite) is a rebuildable view of immutable CAS objects.
//! If projection.sqlite3 is deleted or corrupted, everything can be
//! recovered by walking CAS from signed heads.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::projection::{project_event, project_thread_snapshot, ProjectionDb};
use crate::{ThreadEvent, ThreadSnapshot};

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
#[tracing::instrument(name = "state:rebuild", skip(projection, cas_root, refs_root))]
pub fn rebuild_projection(
    projection: &ProjectionDb,
    cas_root: &Path,
    refs_root: &Path,
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

        // Read signed head → chain_state hash
        let ref_content = std::fs::read_to_string(&head_path)?;
        let ref_value: Value = serde_json::from_str(&ref_content)?;
        let chain_state_hash = ref_value
            .get("target_hash")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        if chain_state_hash.is_empty() {
            continue;
        }

        batch.ensure_started()?;

        // Walk chain history (newest to oldest via prev_chain_state_hash)
        let chain_report = rebuild_chain(projection, cas_root, &chain_root_id, chain_state_hash)?;

        // Update projection_meta to point to the head
        // Read the head chain_state to get updated_at
        if let Some(meta) = head_projection_meta(cas_root, &chain_root_id, chain_state_hash) {
            projection.update_projection_meta(&meta)?;
        }

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
#[tracing::instrument(name = "state:catch_up", skip(projection, cas_root, refs_root))]
pub fn catch_up_projection(
    projection: &ProjectionDb,
    cas_root: &Path,
    refs_root: &Path,
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

        // Read current head
        let ref_content = match std::fs::read_to_string(&head_path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let ref_value: Value = match serde_json::from_str(&ref_content) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let head_hash = ref_value
            .get("target_hash")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if head_hash.is_empty() {
            continue;
        }

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
        if let Some(new_meta) = head_projection_meta(cas_root, &chain_root_id, head_hash) {
            projection.update_projection_meta(&new_meta)?;
        }

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

fn head_projection_meta(
    cas_root: &Path,
    chain_root_id: &str,
    chain_state_hash: &str,
) -> Option<crate::projection::ProjectionMeta> {
    let head_path_buf = lillux::shard_path(cas_root, "objects", chain_state_hash, ".json");
    let cs_json = std::fs::read_to_string(&head_path_buf).ok()?;
    let cs_value = serde_json::from_str::<Value>(&cs_json).ok()?;
    let updated_at = cs_value.get("updated_at").and_then(|v| v.as_str())?;
    Some(crate::projection::ProjectionMeta {
        chain_root_id: chain_root_id.to_string(),
        indexed_chain_state_hash: chain_state_hash.to_string(),
        updated_at: updated_at.to_string(),
    })
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
    let mut report = ChainReport {
        threads: 0,
        events: 0,
    };

    // Walk chain state history head → oldest
    let mut state_hashes = Vec::new();
    let mut current_hash = head_hash.to_string();
    let mut visited: HashSet<String> = HashSet::new();

    while !visited.contains(&current_hash) {
        visited.insert(current_hash.clone());
        state_hashes.push(current_hash.clone());

        let cs_path = lillux::shard_path(cas_root, "objects", &current_hash, ".json");
        let cs_json = match std::fs::read_to_string(&cs_path) {
            Ok(j) => j,
            Err(_) => break,
        };
        let cs_value: Value = match serde_json::from_str(&cs_json) {
            Ok(v) => v,
            Err(_) => break,
        };

        current_hash = cs_value
            .get("prev_chain_state_hash")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if current_hash.is_empty() {
            break;
        }
    }

    // Reverse: oldest → newest so we project in chronological order
    state_hashes.reverse();

    // Collect all event hashes to project (avoid projecting same event twice)
    let mut events_to_project: HashSet<String> = HashSet::new();

    // Walk chain states in order, project thread snapshots and collect events
    for state_hash in &state_hashes {
        let cs_path = lillux::shard_path(cas_root, "objects", state_hash, ".json");
        let cs_json = match std::fs::read_to_string(&cs_path) {
            Ok(j) => j,
            Err(_) => continue,
        };
        let cs_value: Value = match serde_json::from_str(&cs_json) {
            Ok(v) => v,
            Err(_) => continue,
        };

        // Project thread snapshots from this chain_state
        if let Some(threads) = cs_value.get("threads").and_then(|v| v.as_object()) {
            for (_thread_id, entry) in threads {
                if let Some(snap_hash) = entry.get("snapshot_hash").and_then(|v| v.as_str()) {
                    if let Ok(snapshot) = load_thread_snapshot(cas_root, snap_hash) {
                        // Only project the latest snapshot for each thread
                        // (earlier chain states will have older snapshots that get overwritten)
                        project_thread_snapshot(projection, &snapshot, chain_root_id)?;
                        report.threads = report.threads.max(1); // will be counted properly below
                    }
                }
                // Collect last_event_hash as a hint for event walk
                if let Some(event_hash) = entry.get("last_event_hash").and_then(|v| v.as_str()) {
                    if !event_hash.is_empty() {
                        events_to_project.insert(event_hash.to_string());
                    }
                }
            }
        }
    }

    // For events, walk from each thread's last_event_hash toward earlier links
    // via prev_chain_event_hash to get all events in the chain.
    // But we need the chain-level events, so we walk from chain_state's last_event_hash.
    if let Some(last_state_hash) = state_hashes.last() {
        let cs_path = lillux::shard_path(cas_root, "objects", last_state_hash, ".json");
        if let Ok(cs_json) = std::fs::read_to_string(&cs_path) {
            if let Ok(cs_value) = serde_json::from_str::<Value>(&cs_json) {
                // Walk all events from last_event_hash
                if let Some(last_event) = cs_value.get("last_event_hash").and_then(|v| v.as_str()) {
                    let event_count = walk_and_project_events(projection, cas_root, last_event)?;
                    report.events += event_count;
                }
            }
        }
    }

    // Count unique threads
    let conn = projection.connection();
    let count: usize = conn
        .query_row(
            "SELECT COUNT(DISTINCT thread_id) FROM threads WHERE chain_root_id = ?",
            rusqlite::params![chain_root_id],
            |row| row.get(0),
        )
        .unwrap_or(0);
    report.threads = count;

    Ok(report)
}

/// Rebuild only the delta from `from_hash` to `to_hash` for a chain.
fn rebuild_chain_delta(
    projection: &ProjectionDb,
    cas_root: &Path,
    chain_root_id: &str,
    from_hash: &str,
    to_hash: &str,
) -> Result<ChainReport> {
    // Collect chain state hashes from `to_hash` toward earlier links until we reach `from_hash`
    let mut state_hashes = Vec::new();
    let mut current_hash = to_hash.to_string();
    let mut visited: HashSet<String> = HashSet::new();
    let mut found_from = false;

    while !visited.contains(&current_hash) {
        if current_hash == from_hash {
            found_from = true;
            break;
        }
        visited.insert(current_hash.clone());
        state_hashes.push(current_hash.clone());

        let cs_path = lillux::shard_path(cas_root, "objects", &current_hash, ".json");
        let cs_json = match std::fs::read_to_string(&cs_path) {
            Ok(j) => j,
            Err(_) => break,
        };
        let cs_value: Value = match serde_json::from_str(&cs_json) {
            Ok(v) => v,
            Err(_) => break,
        };

        current_hash = cs_value
            .get("prev_chain_state_hash")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if current_hash.is_empty() {
            break;
        }
    }

    if !found_from {
        // Couldn't find from_hash in chain — fall back to full rebuild
        return rebuild_chain(projection, cas_root, chain_root_id, to_hash);
    }

    // Reverse to get chronological order of new states
    state_hashes.reverse();

    let mut report = ChainReport {
        threads: 0,
        events: 0,
    };

    // Project thread snapshots from new chain states. Walk NEWEST state
    // first: with the per-thread dedup below, the first snapshot projected
    // per thread must be its latest — walking oldest-first here regresses a
    // row to the delta's earliest state (a thread that went
    // running → completed inside one delta reads back `running`, and the
    // startup reconciler then finalizes a finished thread as failed).
    let mut seen_threads: HashSet<String> = HashSet::new();
    for state_hash in state_hashes.iter().rev() {
        let cs_path = lillux::shard_path(cas_root, "objects", state_hash, ".json");
        let cs_json = match std::fs::read_to_string(&cs_path) {
            Ok(j) => j,
            Err(_) => continue,
        };
        let cs_value: Value = match serde_json::from_str(&cs_json) {
            Ok(v) => v,
            Err(_) => continue,
        };

        if let Some(threads) = cs_value.get("threads").and_then(|v| v.as_object()) {
            for (thread_id, entry) in threads {
                if seen_threads.contains(thread_id) {
                    continue;
                }
                if let Some(snap_hash) = entry.get("snapshot_hash").and_then(|v| v.as_str()) {
                    if let Ok(snapshot) = load_thread_snapshot(cas_root, snap_hash) {
                        project_thread_snapshot(projection, &snapshot, chain_root_id)?;
                        seen_threads.insert(thread_id.clone());
                        report.threads += 1;
                    }
                }
            }
        }
    }

    // Project new events
    if let Some(last_state_hash) = state_hashes.last() {
        let cs_path = lillux::shard_path(cas_root, "objects", last_state_hash, ".json");
        if let Ok(cs_json) = std::fs::read_to_string(&cs_path) {
            if let Ok(cs_value) = serde_json::from_str::<Value>(&cs_json) {
                // Get the previous state's last_event_hash to know where to start
                let from_cs_path = lillux::shard_path(cas_root, "objects", from_hash, ".json");
                let prev_last_event = std::fs::read_to_string(&from_cs_path)
                    .ok()
                    .and_then(|j| serde_json::from_str::<Value>(&j).ok())
                    .and_then(|v| {
                        v.get("last_event_hash")
                            .and_then(|e| e.as_str())
                            .map(String::from)
                    });

                if let Some(last_event) = cs_value.get("last_event_hash").and_then(|v| v.as_str()) {
                    report.events = walk_and_project_events_from(
                        projection,
                        cas_root,
                        last_event,
                        prev_last_event.as_deref(),
                    )?;
                }
            }
        }
    }

    Ok(report)
}

/// Load a thread snapshot from CAS.
fn load_thread_snapshot(cas_root: &Path, hash: &str) -> Result<ThreadSnapshot> {
    let path = lillux::shard_path(cas_root, "objects", hash, ".json");
    let json = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read thread_snapshot {hash} from CAS"))?;
    let snapshot: ThreadSnapshot = serde_json::from_str(&json)
        .with_context(|| format!("failed to parse thread_snapshot {hash}"))?;
    Ok(snapshot)
}

/// Walk event chain toward earlier links from last_event_hash, projecting all durable events.
/// Returns count of events projected.
fn walk_and_project_events(
    projection: &ProjectionDb,
    cas_root: &Path,
    last_event_hash: &str,
) -> Result<usize> {
    walk_and_project_events_from(projection, cas_root, last_event_hash, None)
}

/// Walk event chain toward earlier links from last_event_hash, optionally stopping at stop_after_hash.
/// Projects all durable events encountered. Returns count of events projected.
fn walk_and_project_events_from(
    projection: &ProjectionDb,
    cas_root: &Path,
    last_event_hash: &str,
    stop_after_hash: Option<&str>,
) -> Result<usize> {
    let mut count = 0;
    let mut current_hash = last_event_hash.to_string();
    let mut visited: HashSet<String> = HashSet::new();

    while !current_hash.is_empty() && !visited.contains(&current_hash) {
        visited.insert(current_hash.clone());

        let path = lillux::shard_path(cas_root, "objects", &current_hash, ".json");
        let json = match std::fs::read_to_string(&path) {
            Ok(j) => j,
            Err(_) => break,
        };
        let value: Value = match serde_json::from_str(&json) {
            Ok(v) => v,
            Err(_) => break,
        };

        // Only project durable events
        let durability = value
            .get("durability")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if durability == "durable" {
            if let Ok(event) = serde_json::from_value::<ThreadEvent>(value.clone()) {
                project_event(projection, &event)?;
                count += 1;
            }
        }

        if stop_after_hash == Some(current_hash.as_str()) {
            break;
        }

        // Follow prev_chain_event_hash
        current_hash = value
            .get("prev_chain_event_hash")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
    }

    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_tracing::test as trace_test;

    fn make_hash(suffix: &str) -> String {
        format!(
            "{:064}",
            suffix
                .as_bytes()
                .iter()
                .fold(0u64, |a, &b| a.wrapping_add(b as u64))
        )
    }

    fn write_object(cas_root: &Path, hash: &str, value: &Value) {
        let path = lillux::shard_path(cas_root, "objects", hash, ".json");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let canonical = lillux::canonical_json(value);
        lillux::atomic_write(&path, canonical.as_bytes()).unwrap();
    }

    fn write_signed_head(refs_root: &Path, chain_root_id: &str, target_hash: &str) {
        let head_path = refs_root
            .join("generic/chains")
            .join(chain_root_id)
            .join("head");
        std::fs::create_dir_all(head_path.parent().unwrap()).unwrap();
        let ref_value = serde_json::json!({
            "schema": 1, "kind": "signed_ref",
            "ref_path": format!("chains/{}/head", chain_root_id),
            "target_hash": target_hash,
            "updated_at": "2026-04-22T00:00:00Z",
            "signer": "test", "signature": "test"
        });
        lillux::atomic_write(&head_path, lillux::canonical_json(&ref_value).as_bytes()).unwrap();
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

        let snap_hash = make_hash("snap");
        let event_hash = make_hash("evt");
        let cs_hash = make_hash("cs");

        let snap = make_snapshot_json("T-root", "T-root", "running");
        let event = make_event_json("T-root", "T-root", 1, 1, None, "thread_created");
        let cs = make_chain_state(
            "T-root",
            None,
            vec![("T-root", &snap_hash, Some(&event_hash), 1, "running")],
            Some(&event_hash),
            1,
        );

        write_object(&cas_root, &snap_hash, &snap);
        write_object(&cas_root, &event_hash, &event);
        write_object(&cas_root, &cs_hash, &cs);
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
            let snap_hash = make_hash(&format!("snap-{chain_id}"));
            let cs_hash = make_hash(&format!("cs-{chain_id}"));
            let snap = make_snapshot_json(chain_id, chain_id, "created");
            let cs = make_chain_state(
                chain_id,
                None,
                vec![(chain_id, &snap_hash, None, 0, "created")],
                None,
                0,
            );
            write_object(&cas_root, &snap_hash, &snap);
            write_object(&cas_root, &cs_hash, &cs);
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

        let snap1_hash = make_hash("snap1");
        let snap2_hash = make_hash("snap2");
        let cs1_hash = make_hash("cs1");
        let cs2_hash = make_hash("cs2");
        let event_hash = make_hash("evt");

        let snap1 = make_snapshot_json("T-root", "T-root", "created");
        let snap2 = make_snapshot_json("T-root", "T-root", "running");
        let event = make_event_json("T-root", "T-root", 1, 1, None, "thread_started");

        let cs1 = make_chain_state(
            "T-root",
            None,
            vec![("T-root", &snap1_hash, None, 0, "created")],
            None,
            0,
        );

        let cs2 = make_chain_state(
            "T-root",
            Some(&cs1_hash),
            vec![("T-root", &snap2_hash, Some(&event_hash), 1, "running")],
            Some(&event_hash),
            1,
        );

        write_object(&cas_root, &snap1_hash, &snap1);
        write_object(&cas_root, &snap2_hash, &snap2);
        write_object(&cas_root, &event_hash, &event);
        write_object(&cas_root, &cs1_hash, &cs1);
        write_object(&cas_root, &cs2_hash, &cs2);
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

        let snap_hash = make_hash("snap");
        let cs_hash = make_hash("cs");
        let snap = make_snapshot_json("T-root", "T-root", "created");
        let cs = make_chain_state(
            "T-root",
            None,
            vec![("T-root", &snap_hash, None, 0, "created")],
            None,
            0,
        );

        write_object(&cas_root, &snap_hash, &snap);
        write_object(&cas_root, &cs_hash, &cs);
        write_signed_head(&refs_root, "T-root", &cs_hash);

        let proj_path = tmp.path().join("projection.db");
        let proj = ProjectionDb::open(&proj_path).unwrap();

        // Set projection_meta to match current head
        let meta = crate::projection::ProjectionMeta {
            chain_root_id: "T-root".to_string(),
            indexed_chain_state_hash: cs_hash.clone(),
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

        let snap1_hash = make_hash("snap1");
        let snap2_hash = make_hash("snap2");
        let cs1_hash = make_hash("cs1");
        let cs2_hash = make_hash("cs2");
        let event_hash = make_hash("evt");

        let snap1 = make_snapshot_json("T-root", "T-root", "created");
        let snap2 = make_snapshot_json("T-root", "T-root", "running");
        let event = make_event_json("T-root", "T-root", 1, 1, None, "thread_started");

        let cs1 = make_chain_state(
            "T-root",
            None,
            vec![("T-root", &snap1_hash, None, 0, "created")],
            None,
            0,
        );
        let cs2 = make_chain_state(
            "T-root",
            Some(&cs1_hash),
            vec![("T-root", &snap2_hash, Some(&event_hash), 1, "running")],
            Some(&event_hash),
            1,
        );

        write_object(&cas_root, &snap1_hash, &snap1);
        write_object(&cas_root, &snap2_hash, &snap2);
        write_object(&cas_root, &event_hash, &event);
        write_object(&cas_root, &cs1_hash, &cs1);
        write_object(&cas_root, &cs2_hash, &cs2);
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

        let snap1_hash = make_hash("m-snap1");
        let snap2_hash = make_hash("m-snap2");
        let snap3_hash = make_hash("m-snap3");
        let cs1_hash = make_hash("m-cs1");
        let cs2_hash = make_hash("m-cs2");
        let cs3_hash = make_hash("m-cs3");

        let snap1 = make_snapshot_json("T-multi", "T-multi", "created");
        let snap2 = make_snapshot_json("T-multi", "T-multi", "running");
        let snap3 = make_snapshot_json("T-multi", "T-multi", "completed");

        let cs1 = make_chain_state(
            "T-multi",
            None,
            vec![("T-multi", &snap1_hash, None, 0, "created")],
            None,
            0,
        );
        let cs2 = make_chain_state(
            "T-multi",
            Some(&cs1_hash),
            vec![("T-multi", &snap2_hash, None, 0, "running")],
            None,
            1,
        );
        let cs3 = make_chain_state(
            "T-multi",
            Some(&cs2_hash),
            vec![("T-multi", &snap3_hash, None, 0, "completed")],
            None,
            2,
        );

        write_object(&cas_root, &snap1_hash, &snap1);
        write_object(&cas_root, &snap2_hash, &snap2);
        write_object(&cas_root, &snap3_hash, &snap3);
        write_object(&cas_root, &cs1_hash, &cs1);
        write_object(&cas_root, &cs2_hash, &cs2);
        write_object(&cas_root, &cs3_hash, &cs3);
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
            let snap_hash = make_hash(&format!("batch-snap-{chain_id}"));
            let cs_hash = make_hash(&format!("batch-cs-{chain_id}"));
            let snap = make_snapshot_json(chain_id, chain_id, "created");
            let cs = make_chain_state(
                chain_id,
                None,
                vec![(chain_id, &snap_hash, None, 0, "created")],
                None,
                0,
            );

            write_object(&cas_root, &snap_hash, &snap);
            write_object(&cas_root, &cs_hash, &cs);
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
            let snap_hash = make_hash(&format!("rollback-snap-{chain_id}"));
            let cs_hash = make_hash(&format!("rollback-cs-{chain_id}"));
            let snap = make_snapshot_json(chain_id, chain_id, "created");
            let cs = make_chain_state(
                chain_id,
                None,
                vec![(chain_id, &snap_hash, None, 0, "created")],
                None,
                0,
            );

            write_object(&cas_root, &snap_hash, &snap);
            write_object(&cas_root, &cs_hash, &cs);
            write_signed_head(&refs_root, chain_id, &cs_hash);
        }

        let bad_snap_hash = make_hash("rollback-snap-T-c");
        let bad_event_hash = make_hash("rollback-event-T-c");
        let bad_cs_hash = make_hash("rollback-cs-T-c");
        let bad_snap = make_snapshot_json("T-c", "T-c", "created");
        let bad_event = make_event_json(
            "T-c",
            "T-c",
            1,
            1,
            Some("not-a-valid-hash"),
            "thread_started",
        );
        let bad_cs = make_chain_state(
            "T-c",
            None,
            vec![("T-c", &bad_snap_hash, Some(&bad_event_hash), 1, "created")],
            Some(&bad_event_hash),
            1,
        );
        write_object(&cas_root, &bad_snap_hash, &bad_snap);
        write_object(&cas_root, &bad_event_hash, &bad_event);
        write_object(&cas_root, &bad_cs_hash, &bad_cs);
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
