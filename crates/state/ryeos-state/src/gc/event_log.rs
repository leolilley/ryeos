//! GC event logging — JSONL append-only log for GC runs.
//!
//! One JSON line per GC run at `runtime_state_dir/logs/gc.jsonl`.
//! Provides operational observability: when did GC run, how much did it free.

#[cfg(test)]
use std::fs;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// A single GC run event.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GcEvent {
    pub timestamp: String,
    pub dry_run: bool,
    pub compact: bool,
    pub roots_walked: usize,
    pub reachable_objects: usize,
    pub reachable_blobs: usize,
    pub deleted_objects: usize,
    pub deleted_blobs: usize,
    pub deleted_runtime_files: usize,
    /// Schedule-fire JSONL lines dropped by the retention sweep.
    #[serde(default)]
    pub deleted_fire_records: usize,
    /// Terminal rows dropped from the scheduler fire projection.
    #[serde(default)]
    pub deleted_fire_projection_rows: usize,
    /// Terminal sync-job rows dropped by the retention sweep.
    #[serde(default)]
    pub deleted_sync_jobs: usize,
    /// Sync-job attempt rows dropped alongside their retired jobs.
    #[serde(default)]
    pub deleted_sync_job_attempts: usize,
    #[serde(default)]
    pub reaped_seats: usize,
    #[serde(default)]
    pub terminal_chain_candidates: usize,
    #[serde(default)]
    pub retired_terminal_chains: usize,
    #[serde(default)]
    pub deleted_thread_projection_rows: usize,
    #[serde(default)]
    pub deleted_thread_runtime_rows: usize,
    #[serde(default)]
    pub deleted_thread_runtime_files: usize,
    #[serde(default)]
    pub pending_retirements_recovered: usize,
    #[serde(default)]
    pub retired_durable_cas_uploads: usize,
    pub freed_bytes: u64,
    pub snapshots_compacted: usize,
    pub duration_ms: u64,
}

impl GcEvent {
    /// Build from a GC result.
    pub fn from_result(
        result: &super::GcResult,
        dry_run: bool,
        compact: bool,
        started_at: std::time::Instant,
    ) -> Self {
        Self {
            timestamp: lillux::time::iso8601_now(),
            dry_run,
            compact,
            roots_walked: result.roots_walked,
            reachable_objects: result.reachable_objects,
            reachable_blobs: result.reachable_blobs,
            deleted_objects: result.deleted_objects,
            deleted_blobs: result.deleted_blobs,
            deleted_runtime_files: result.deleted_runtime_files,
            deleted_fire_records: result.deleted_fire_records,
            deleted_fire_projection_rows: result.deleted_fire_projection_rows,
            deleted_sync_jobs: result.deleted_sync_jobs,
            deleted_sync_job_attempts: result.deleted_sync_job_attempts,
            reaped_seats: result.reaped_seats,
            terminal_chain_candidates: result.terminal_chain_candidates,
            retired_terminal_chains: result.retired_terminal_chains,
            deleted_thread_projection_rows: result.deleted_thread_projection_rows,
            deleted_thread_runtime_rows: result.deleted_thread_runtime_rows,
            deleted_thread_runtime_files: result.deleted_thread_runtime_files,
            pending_retirements_recovered: result.pending_retirements_recovered,
            retired_durable_cas_uploads: result.retired_durable_cas_uploads,
            freed_bytes: result.freed_bytes,
            snapshots_compacted: result
                .compaction
                .as_ref()
                .map(|c| c.snapshots_removed)
                .unwrap_or(0),
            duration_ms: started_at.elapsed().as_millis() as u64,
        }
    }
}

/// Append a GC event to the JSONL log.
///
/// Creates the log directory and file if they don't exist.
pub fn append_event(runtime_state_dir: &Path, event: &GcEvent) -> Result<()> {
    let runtime_directory = lillux::PinnedDirectory::open_or_create(runtime_state_dir)
        .context("failed to open runtime-state directory for GC event log")?;
    append_event_in_directory(&runtime_directory, event)
}

/// Append a GC event beneath one already-pinned runtime root.
pub fn append_event_in_directory(
    runtime_directory: &lillux::PinnedDirectory,
    event: &GcEvent,
) -> Result<()> {
    let log_directory = runtime_directory
        .open_or_create_child(std::ffi::OsStr::new("logs"), 0o700)
        .context("failed to create GC log directory")?;
    let name = std::ffi::OsStr::new("gc.jsonl");
    let lock = lillux::ExclusiveFileLock::acquire_in(&log_directory, name)?;
    let mut file = lock
        .open_target_append_create()
        .context("failed to open GC event log")?;

    let line = serde_json::to_string(event).context("failed to serialize GC event")?;

    writeln!(file, "{}", line).context("failed to write GC event")?;
    file.sync_data().context("failed to sync GC event log")?;
    lock.sync_parent()?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_and_read_event() {
        let tmp = tempfile::tempdir().unwrap();
        let runtime_state_dir = tmp.path().join("state");

        let event = GcEvent {
            timestamp: "2026-04-23T00:00:00Z".to_string(),
            dry_run: false,
            compact: false,
            roots_walked: 3,
            reachable_objects: 100,
            reachable_blobs: 20,
            deleted_objects: 10,
            deleted_blobs: 5,
            deleted_runtime_files: 0,
            deleted_fire_records: 0,
            deleted_fire_projection_rows: 0,
            deleted_sync_jobs: 0,
            deleted_sync_job_attempts: 0,
            reaped_seats: 0,
            terminal_chain_candidates: 0,
            retired_terminal_chains: 0,
            deleted_thread_projection_rows: 0,
            deleted_thread_runtime_rows: 0,
            deleted_thread_runtime_files: 0,
            pending_retirements_recovered: 0,
            retired_durable_cas_uploads: 0,
            freed_bytes: 4096,
            snapshots_compacted: 0,
            duration_ms: 150,
        };

        append_event(&runtime_state_dir, &event).unwrap();

        let content = fs::read_to_string(runtime_state_dir.join("logs/gc.jsonl")).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 1);

        let parsed: GcEvent = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(parsed.deleted_objects, 10);
        assert_eq!(parsed.freed_bytes, 4096);
    }

    #[test]
    fn append_multiple_events() {
        let tmp = tempfile::tempdir().unwrap();
        let runtime_state_dir = tmp.path().join("state");

        for i in 0..3 {
            let event = GcEvent {
                timestamp: format!("2026-04-23T00:0{}:00Z", i),
                dry_run: i == 2,
                compact: false,
                roots_walked: 1,
                reachable_objects: 50,
                reachable_blobs: 10,
                deleted_objects: 5,
                deleted_blobs: 2,
                deleted_runtime_files: 0,
                deleted_fire_records: 0,
                deleted_fire_projection_rows: 0,
                deleted_sync_jobs: 0,
                deleted_sync_job_attempts: 0,
                reaped_seats: 0,
                terminal_chain_candidates: 0,
                retired_terminal_chains: 0,
                deleted_thread_projection_rows: 0,
                deleted_thread_runtime_rows: 0,
                deleted_thread_runtime_files: 0,
                pending_retirements_recovered: 0,
                retired_durable_cas_uploads: 0,
                freed_bytes: 1024,
                snapshots_compacted: 0,
                duration_ms: 100,
            };
            append_event(&runtime_state_dir, &event).unwrap();
        }

        let content = fs::read_to_string(runtime_state_dir.join("logs/gc.jsonl")).unwrap();
        assert_eq!(content.lines().count(), 3);
    }
}
