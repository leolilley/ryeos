//! GC event logging — JSONL append-only log for GC runs.
//!
//! One JSON line per GC run at `runtime_state_dir/logs/gc.jsonl`.
//! Provides operational observability: when did GC run, how much did it free.

use std::fs::{self, OpenOptions};
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
    /// Terminal sync-job rows dropped by the retention sweep.
    #[serde(default)]
    pub deleted_sync_jobs: usize,
    /// Sync-job attempt rows dropped alongside their retired jobs.
    #[serde(default)]
    pub deleted_sync_job_attempts: usize,
    #[serde(default)]
    pub reaped_seats: usize,
    #[serde(default)]
    pub retired_service_chains: usize,
    #[serde(default)]
    pub deleted_service_chain_rows: usize,
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
            deleted_sync_jobs: result.deleted_sync_jobs,
            deleted_sync_job_attempts: result.deleted_sync_job_attempts,
            reaped_seats: result.reaped_seats,
            retired_service_chains: result.retired_service_chains,
            deleted_service_chain_rows: result.deleted_service_chain_rows,
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
    let log_dir = runtime_state_dir.join("logs");
    let log_path = log_dir.join("gc.jsonl");

    fs::create_dir_all(&log_dir).context("failed to create GC log directory")?;

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .context("failed to open GC event log")?;

    let line = serde_json::to_string(event).context("failed to serialize GC event")?;

    writeln!(file, "{}", line).context("failed to write GC event")?;

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
            deleted_sync_jobs: 0,
            deleted_sync_job_attempts: 0,
            reaped_seats: 0,
            retired_service_chains: 0,
            deleted_service_chain_rows: 0,
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
                deleted_sync_jobs: 0,
                deleted_sync_job_attempts: 0,
                reaped_seats: 0,
                retired_service_chains: 0,
                deleted_service_chain_rows: 0,
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
