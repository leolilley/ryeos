//! Phase 1: Prune stale cache and excess execution records.
//!
//! Operates on ephemeral state only — no GC lock needed.

use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use super::RetentionPolicy;

/// Result of the prune phase.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PruneResult {
    pub cache_entries_removed: usize,
    pub execution_records_removed: usize,
    pub freed_bytes: u64,
}

/// Run the prune phase.
///
/// - Deletes stale materialized cache snapshots older than `policy.cache_max_age_hours`
/// - Deletes excess execution metadata beyond retention limits
///
/// `cas_root` is used for CAS-internal paths (epochs).
/// `state_dir` is used for ephemeral runtime state (cache, executions).
pub fn run_prune(
    cas_root: &Path,
    state_dir: &Path,
    policy: &RetentionPolicy,
    dry_run: bool,
) -> Result<PruneResult> {
    let mut result = PruneResult::default();

    // Prune materialized snapshot cache via MaterializationCache
    let snapshot_cache_dir = state_dir.join("cache").join("snapshots");
    let mat_cache = crate::execution::cache::MaterializationCache::new(snapshot_cache_dir.clone());
    let max_age = Duration::from_secs(policy.cache_max_age_hours * 3600);
    let now = SystemTime::now();
    if let Ok(cached_hashes) = mat_cache.list() {
        for hash in &cached_hashes {
            if !mat_cache.has(hash) {
                continue;
            }
            let dir = mat_cache.cache_dir(hash);
            let modified = fs::metadata(&dir).and_then(|m| m.modified()).unwrap_or(now);
            if let Ok(age) = now.duration_since(modified) {
                if age > max_age {
                    let size = dir_size(&dir);
                    if !dry_run {
                        let _ = mat_cache.evict(hash);
                    }
                    result.cache_entries_removed += 1;
                    result.freed_bytes += size;
                    tracing::debug!(
                        hash,
                        age_hours = age.as_secs() / 3600,
                        "pruned snapshot cache entry"
                    );
                }
            }
        }
    }

    // Clean up stale epochs (older than 24 hours)
    let stale_cleaned = super::epochs::cleanup_stale_epochs(cas_root, 86400)?;
    if stale_cleaned > 0 {
        tracing::info!(cleaned = stale_cleaned, "cleaned stale writer epochs");
    }

    // Rotate GC event log when it exceeds 10MB
    let gc_log = cas_root.join("logs").join("gc.jsonl");
    if gc_log.is_file() {
        if let Ok(meta) = gc_log.metadata() {
            if meta.len() > 10 * 1024 * 1024 {
                let rotated = cas_root.join("logs").join("gc.jsonl.old");
                let _ = fs::rename(&gc_log, &rotated);
            }
        }
    }

    // Prune execution records
    let exec_dir = state_dir.join("executions");
    if exec_dir.is_dir() {
        result.execution_records_removed += prune_execution_records(
            &exec_dir,
            policy.max_success_executions,
            policy.max_failure_executions,
            dry_run,
            &mut result.freed_bytes,
        )?;
    }

    tracing::info!(
        cache_removed = result.cache_entries_removed,
        exec_removed = result.execution_records_removed,
        freed_bytes = result.freed_bytes,
        dry_run,
        "prune phase complete"
    );

    Ok(result)
}

/// Prune execution records beyond retention limits.
///
/// Each subdirectory under `exec_dir` is a project+graph combination.
/// Within each, JSON files are sorted by timestamp and excess are removed.
fn prune_execution_records(
    exec_dir: &Path,
    max_success: usize,
    max_failure: usize,
    dry_run: bool,
    freed_bytes: &mut u64,
) -> Result<usize> {
    let mut total_removed = 0;

    for entry in fs::read_dir(exec_dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let mut records: Vec<(String, u64, bool)> = Vec::new(); // (filename, size, is_success)

        for record_entry in fs::read_dir(&path)? {
            let record_entry = record_entry?;
            let record_path = record_entry.path();
            if !record_path.is_file() {
                continue;
            }

            let size = record_path.metadata().map(|m| m.len()).unwrap_or(0);
            let name = record_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();

            // Determine success/failure from filename convention
            let is_success = !name.contains("fail") && !name.contains("error");
            records.push((name, size, is_success));
        }

        // Sort by name (which includes timestamp) — newest last
        records.sort_by(|a, b| a.0.cmp(&b.0));

        let success_records: Vec<_> = records.iter().filter(|r| r.2).collect();
        let failure_records: Vec<_> = records.iter().filter(|r| !r.2).collect();

        // Remove oldest excess success records
        if success_records.len() > max_success {
            let excess = success_records.len() - max_success;
            for record in &success_records[..excess] {
                let file_path = path.join(&record.0);
                if !dry_run {
                    fs::remove_file(&file_path)?;
                }
                *freed_bytes += record.1;
                total_removed += 1;
            }
        }

        // Remove oldest excess failure records
        if failure_records.len() > max_failure {
            let excess = failure_records.len() - max_failure;
            for record in &failure_records[..excess] {
                let file_path = path.join(&record.0);
                if !dry_run {
                    fs::remove_file(&file_path)?;
                }
                *freed_bytes += record.1;
                total_removed += 1;
            }
        }
    }

    Ok(total_removed)
}

/// Compute total size of a directory tree.
fn dir_size(path: &Path) -> u64 {
    let mut total = 0u64;
    if let Ok(entries) = fs::read_dir(path) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_file() {
                total += p.metadata().map(|m| m.len()).unwrap_or(0);
            } else if p.is_dir() {
                total += dir_size(&p);
            }
        }
    }
    total
}
