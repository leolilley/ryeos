//! Phase 3: Mark-and-sweep.
//!
//! Mark: collect root hashes from all refs (centralized via
//! `RefStore::gc_root_hashes()`) → BFS walk child hashes using
//! kind-aware `extract_child_hashes()`.
//! Sweep: walk `blobs/` and `objects/` → delete unreachable files.
//!
//! Supports incremental marking via mark_cache: if the root set is
//! unchanged since last run, the cached reachable set is reused.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::cas::{self, CasStore};
use crate::refs::RefStore;

use super::epochs;
use super::mark_cache;

/// Result of the sweep phase.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SweepResult {
    pub roots_collected: usize,
    pub reachable_objects: usize,
    pub blobs_removed: usize,
    pub objects_removed: usize,
    pub freed_bytes: u64,
    pub mark_cache_hit: bool,
}

/// Run the mark-and-sweep phase.
pub fn run_sweep(
    cas: &CasStore,
    refs: &RefStore,
    cas_root: &Path,
    dry_run: bool,
) -> Result<SweepResult> {
    let mut result = SweepResult::default();

    // ── Mark phase ──────────────────────────────────────────────────

    let mut roots: HashSet<String> = refs.gc_root_hashes()?.into_iter().collect();

    if let Ok(active) = epochs::list_active_epochs(cas_root) {
        for epoch in &active {
            for hash in &epoch.root_hashes {
                roots.insert(hash.clone());
            }
        }
    }

    result.roots_collected = roots.len();

    // Build comparison roots excluding internal GC refs (mark cache object)
    // to avoid self-referential root-set churn.
    let mark_cache_hash = refs
        .read_ref("internal/last_mark_cache")
        .ok()
        .flatten();
    let comparison_roots: Vec<String> = {
        let mut v: Vec<String> = roots
            .iter()
            .filter(|h| mark_cache_hash.as_deref() != Some(h.as_str()))
            .cloned()
            .collect();
        v.sort();
        v
    };

    let reachable = if let Some(cached) = mark_cache::load_mark_cache(cas, refs) {
        let mut cached_sorted = cached.root_hashes.clone();
        cached_sorted.sort();
        if cached_sorted == comparison_roots {
            result.mark_cache_hit = true;
            tracing::debug!("mark cache hit — reusing previous reachable set");
            cached.reachable
        } else {
            tracing::debug!("mark cache miss — roots changed, running full BFS");
            mark_reachable(cas, &roots)?
        }
    } else {
        tracing::debug!("no mark cache — running full BFS");
        mark_reachable(cas, &roots)?
    };

    result.reachable_objects = reachable.len();

    // ── Sweep phase ─────────────────────────────────────────────────

    let grace_time = epochs::oldest_epoch_time(cas_root).unwrap_or_else(|_| SystemTime::now());

    let objects_root = cas_root.join("objects");
    if objects_root.is_dir() {
        sweep_sharded_dir(
            &objects_root,
            &reachable,
            grace_time,
            dry_run,
            &mut result.objects_removed,
            &mut result.freed_bytes,
        )?;
    }

    let blobs_root = cas_root.join("blobs");
    if blobs_root.is_dir() {
        sweep_sharded_dir(
            &blobs_root,
            &reachable,
            grace_time,
            dry_run,
            &mut result.blobs_removed,
            &mut result.freed_bytes,
        )?;
    }

    if !dry_run {
        if let Err(e) = mark_cache::save_mark_cache(cas, refs, &comparison_roots, &reachable) {
            tracing::warn!(error = %e, "failed to save mark cache");
        }
    }

    tracing::info!(
        roots = result.roots_collected,
        reachable = result.reachable_objects,
        blobs_removed = result.blobs_removed,
        objects_removed = result.objects_removed,
        freed_bytes = result.freed_bytes,
        cache_hit = result.mark_cache_hit,
        dry_run,
        "sweep phase complete"
    );

    Ok(result)
}

/// BFS walk from root hashes, following child hashes via `extract_child_hashes`.
fn mark_reachable(cas: &CasStore, roots: &HashSet<String>) -> Result<HashSet<String>> {
    let mut reachable = HashSet::new();
    let mut queue: Vec<String> = roots.iter().cloned().collect();

    while let Some(hash) = queue.pop() {
        if !reachable.insert(hash.clone()) {
            continue;
        }

        if let Ok(Some(obj)) = cas.get_object(&hash) {
            let children = cas::extract_child_hashes(&obj);
            for child in children {
                if !reachable.contains(&child) {
                    queue.push(child);
                }
            }
        }
    }

    Ok(reachable)
}

/// Sweep a sharded directory (blobs/ or objects/), deleting unreachable files.
fn sweep_sharded_dir(
    root: &Path,
    reachable: &HashSet<String>,
    grace_time: SystemTime,
    dry_run: bool,
    removed_count: &mut usize,
    freed_bytes: &mut u64,
) -> Result<()> {
    for l1 in read_dir_sorted(root)? {
        if !l1.is_dir() {
            continue;
        }
        for l2 in read_dir_sorted(&l1)? {
            if !l2.is_dir() {
                continue;
            }
            for file_path in read_dir_sorted(&l2)? {
                if !file_path.is_file() {
                    continue;
                }

                let file_name = file_path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
                if file_name.is_empty() {
                    continue;
                }

                if reachable.contains(file_name) {
                    continue;
                }

                if let Ok(metadata) = file_path.metadata() {
                    if let Ok(modified) = metadata.modified() {
                        if modified > grace_time {
                            continue;
                        }
                    }
                    let size = metadata.len();
                    if !dry_run {
                        fs::remove_file(&file_path)?;
                    }
                    *removed_count += 1;
                    *freed_bytes += size;
                }
            }
        }
    }

    Ok(())
}

/// Read a directory and return sorted paths.
fn read_dir_sorted(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut paths: Vec<PathBuf> = fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .collect();
    paths.sort();
    Ok(paths)
}
