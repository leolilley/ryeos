//! Phase 3: Mark-and-sweep.
//!
//! Mark: collect root hashes from all refs → BFS walk child hashes
//! using kind-aware `extract_child_hashes()`.
//! Sweep: walk `blobs/` and `objects/` → delete unreachable files.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::cas::{self, CasStore};
use crate::refs::RefStore;

use super::epochs;

/// Result of the sweep phase.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SweepResult {
    pub roots_collected: usize,
    pub reachable_objects: usize,
    pub blobs_removed: usize,
    pub objects_removed: usize,
    pub freed_bytes: u64,
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

    // Collect root hashes from all ref sources
    let mut roots = HashSet::new();

    // Project heads
    collect_project_head_roots(cas_root, &mut roots)?;

    // User-space heads
    collect_user_space_roots(cas_root, &mut roots)?;

    // Pin refs
    if let Ok(pins) = refs.list_pins() {
        for name in &pins {
            if let Ok(Some(hash)) = refs.read_pin(name) {
                roots.insert(hash);
            }
        }
    }

    // In-flight epoch roots
    if let Ok(active) = epochs::list_active_epochs(cas_root) {
        for epoch in &active {
            for hash in &epoch.root_hashes {
                roots.insert(hash.clone());
            }
        }
    }

    result.roots_collected = roots.len();

    // BFS walk from roots to find all reachable hashes
    let reachable = mark_reachable(cas, &roots)?;
    result.reachable_objects = reachable.len();

    // ── Sweep phase ─────────────────────────────────────────────────

    // Grace period: skip files newer than oldest active epoch
    let grace_time = epochs::oldest_epoch_time(cas_root)
        .unwrap_or_else(|_| SystemTime::now());

    // Sweep objects/
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

    // Sweep blobs/
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

    tracing::info!(
        roots = result.roots_collected,
        reachable = result.reachable_objects,
        blobs_removed = result.blobs_removed,
        objects_removed = result.objects_removed,
        freed_bytes = result.freed_bytes,
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
            continue; // Already visited
        }

        // Try as object first
        if let Ok(Some(obj)) = cas.get_object(&hash) {
            let children = cas::extract_child_hashes(&obj);
            for child in children {
                if !reachable.contains(&child) {
                    queue.push(child);
                }
            }
        }
        // Blobs are leaf nodes — no children to follow
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
    // Walk shard dirs: root/XX/YY/HASH[.json]
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

                // Extract hash from filename (strip .json extension if present)
                let file_name = file_path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("");
                if file_name.is_empty() {
                    continue;
                }

                if reachable.contains(file_name) {
                    continue; // Reachable — keep
                }

                // Grace period check
                if let Ok(metadata) = file_path.metadata() {
                    if let Ok(modified) = metadata.modified() {
                        if modified > grace_time {
                            continue; // Too new — skip
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

/// Collect project head hashes from all principals.
fn collect_project_head_roots(cas_root: &Path, roots: &mut HashSet<String>) -> Result<()> {
    for principal_entry in read_dir_sorted(cas_root)? {
        if !principal_entry.is_dir() {
            continue;
        }
        let projects_dir = principal_entry.join("refs").join("projects");
        if !projects_dir.is_dir() {
            continue;
        }
        for project_entry in read_dir_sorted(&projects_dir)? {
            let head = project_entry.join("head");
            if head.is_file() {
                if let Ok(hash) = fs::read_to_string(&head) {
                    let hash = hash.trim().to_string();
                    if !hash.is_empty() {
                        roots.insert(hash);
                    }
                }
            }
        }
    }
    Ok(())
}

/// Collect user-space head hashes from all principals.
fn collect_user_space_roots(cas_root: &Path, roots: &mut HashSet<String>) -> Result<()> {
    for principal_entry in read_dir_sorted(cas_root)? {
        if !principal_entry.is_dir() {
            continue;
        }
        let head = principal_entry.join("refs").join("user-space").join("head");
        if head.is_file() {
            if let Ok(hash) = fs::read_to_string(&head) {
                let hash = hash.trim().to_string();
                if !hash.is_empty() {
                    roots.insert(hash);
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
