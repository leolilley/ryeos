use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

use anyhow::Result;
use serde::Serialize;

use super::{GCParams, GCResult};

#[derive(Debug, Serialize)]
pub struct GCEvent {
    pub timestamp: String,
    pub node_id: String,
    pub dry_run: bool,
    pub aggressive: bool,
    pub roots_collected: usize,
    pub reachable_objects: usize,
    pub blobs_removed: usize,
    pub objects_removed: usize,
    pub cache_entries_pruned: usize,
    pub execution_records_pruned: usize,
    pub total_freed_bytes: u64,
    pub duration_ms: u64,
}

impl GCEvent {
    pub fn from_result(result: &GCResult, params: &GCParams, node_id: &str) -> Self {
        Self {
            timestamp: chrono::Utc::now().to_rfc3339(),
            node_id: node_id.to_string(),
            dry_run: params.dry_run,
            aggressive: params.aggressive,
            roots_collected: result.sweep.roots_collected,
            reachable_objects: result.sweep.reachable_objects,
            blobs_removed: result.sweep.blobs_removed,
            objects_removed: result.sweep.objects_removed,
            cache_entries_pruned: result.prune.cache_entries_removed,
            execution_records_pruned: result.prune.execution_records_removed,
            total_freed_bytes: result.total_freed_bytes,
            duration_ms: result.duration_ms,
        }
    }
}

pub fn append_event(cas_root: &Path, event: &GCEvent) -> Result<()> {
    let log_dir = cas_root.join("logs");
    fs::create_dir_all(&log_dir)?;
    let log_path = log_dir.join("gc.jsonl");

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;

    let line = serde_json::to_string(event)?;
    writeln!(file, "{}", line)?;
    Ok(())
}
