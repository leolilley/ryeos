//! Mark-and-sweep garbage collection for CAS objects.
//!
//! Walks signed heads (chains + projects) using the shared reachability
//! traversal, then sweeps unreachable objects and blobs from disk.
//!
//! Requires daemon stopped (D16).

use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::reachability;

/// Report from a garbage collection run.
#[derive(Debug, Clone, Default, Serialize)]
pub struct GcResult {
    pub roots_walked: usize,
    pub reachable_objects: usize,
    pub reachable_blobs: usize,
    pub deleted_objects: usize,
    pub deleted_blobs: usize,
    pub freed_bytes: u64,
}

/// Mark-and-sweep GC.
///
/// Mark: collect_reachable() from all signed heads.
/// Sweep: walk objects/ and blobs/ dirs, delete unreachable.
pub fn run_gc(
    cas_root: &Path,
    refs_root: &Path,
    dry_run: bool,
) -> Result<GcResult> {
    let reachable = reachability::collect_reachable(cas_root, refs_root)?;

    let mut result = GcResult::default();
    result.roots_walked = reachable.chain_root_ids.len() + reachable.project_hashes.len();
    result.reachable_objects = reachable.object_hashes.len();
    result.reachable_blobs = reachable.blob_hashes.len();

    // Sweep objects/
    sweep_sharded_dir(cas_root, "objects", ".json", &reachable.object_hashes, dry_run, &mut result)?;

    // Sweep blobs/
    sweep_sharded_dir(cas_root, "blobs", "", &reachable.blob_hashes, dry_run, &mut result)?;

    Ok(result)
}

/// Sweep a sharded directory, deleting files not in the reachable set.
fn sweep_sharded_dir(
    cas_root: &Path,
    namespace: &str,
    ext: &str,
    reachable: &std::collections::HashSet<String>,
    dry_run: bool,
    result: &mut GcResult,
) -> Result<()> {
    let dir = cas_root.join(namespace);
    if !dir.is_dir() {
        return Ok(());
    }

    // Walk the 2-level sharded directory structure (xx/xx...)
    for shard1 in std::fs::read_dir(&dir)
        .with_context(|| format!("failed to read {}", namespace))?
    {
        let shard1 = shard1.context("failed to read shard entry")?;
        if !shard1.file_type()?.is_dir() {
            continue;
        }

        for shard2 in std::fs::read_dir(shard1.path())
            .with_context(|| format!("failed to read shard subdirectory"))?
        {
            let shard2 = shard2.context("failed to read shard sub-entry")?;
            if !shard2.file_type()?.is_file() {
                continue;
            }

            // Extract hash from filename
            let filename = shard2.file_name();
            let filename_str = filename.to_string_lossy();
            // Strip extension to get the hash
            let hash = if ext.is_empty() {
                filename_str.to_string()
            } else if let Some(stripped) = filename_str.strip_suffix(ext) {
                stripped.to_string()
            } else {
                continue;
            };

            if !reachable.contains(&hash) {
                let file_size = shard2.metadata().map(|m| m.len()).unwrap_or(0);

                if dry_run {
                    tracing::info!(
                        namespace = namespace,
                        hash = %&hash[..16.min(hash.len())],
                        size = file_size,
                        "would delete (dry run)"
                    );
                } else {
                    std::fs::remove_file(shard2.path())
                        .with_context(|| format!("failed to delete {}", shard2.path().display()))?;
                }

                if namespace == "blobs" {
                    result.deleted_blobs += 1;
                } else {
                    result.deleted_objects += 1;
                }
                result.freed_bytes += file_size;
            }
        }

        // Also clean up empty shard directories
        if shard1.path().is_dir() {
            let remaining = std::fs::read_dir(shard1.path())
                .and_then(|entries| Ok(entries.count()))
                .unwrap_or(0);
            if remaining == 0 && !dry_run {
                let _ = std::fs::remove_dir(shard1.path());
            }
        }
    }

    Ok(())
}
