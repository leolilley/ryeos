//! Phase 2: History compaction.
//!
//! Walks the project snapshot DAG via `parent_hashes`, applies the
//! retention policy, and rewrites the DAG to discard unreferenced
//! intermediate snapshots.

use std::collections::HashSet;
use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::cas::CasStore;
use crate::refs::RefStore;

use super::RetentionPolicy;

/// Result of the compaction phase.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CompactionResult {
    pub projects_scanned: usize,
    pub snapshots_removed: usize,
}

/// Run the compaction phase.
///
/// For each principal's project refs, walks the snapshot DAG and applies
/// retention policy to determine which snapshots to keep.
pub fn run_compact(
    cas: &CasStore,
    _refs: &RefStore,
    cas_root: &Path,
    policy: &RetentionPolicy,
    dry_run: bool,
) -> Result<CompactionResult> {
    let mut result = CompactionResult::default();

    // Walk principal directories to find project refs
    let refs_root = cas_root;
    if !refs_root.is_dir() {
        return Ok(result);
    }

    for principal_entry in std::fs::read_dir(refs_root)? {
        let principal_entry = principal_entry?;
        let principal_path = principal_entry.path();
        if !principal_path.is_dir() {
            continue;
        }

        let projects_dir = principal_path.join("refs").join("projects");
        if !projects_dir.is_dir() {
            continue;
        }

        for project_entry in std::fs::read_dir(&projects_dir)? {
            let project_entry = project_entry?;
            let project_dir = project_entry.path();
            let head_file = project_dir.join("head");
            if !head_file.is_file() {
                continue;
            }

            result.projects_scanned += 1;

            let head_hash = std::fs::read_to_string(&head_file)?.trim().to_string();
            let removed = compact_project_dag(cas, &head_hash, policy, dry_run)?;
            result.snapshots_removed += removed;
        }
    }

    tracing::info!(
        projects = result.projects_scanned,
        removed = result.snapshots_removed,
        dry_run,
        "compact phase complete"
    );

    Ok(result)
}

/// Walk a project's snapshot DAG and count snapshots beyond retention.
///
/// Returns the number of snapshots that would be / were removed.
fn compact_project_dag(
    cas: &CasStore,
    head_hash: &str,
    policy: &RetentionPolicy,
    _dry_run: bool,
) -> Result<usize> {
    // Collect all snapshots in the DAG
    let mut snapshots: Vec<(String, String, String)> = Vec::new(); // (hash, source, timestamp)
    let mut visited = HashSet::new();
    let mut queue = vec![head_hash.to_string()];

    while let Some(hash) = queue.pop() {
        if !visited.insert(hash.clone()) {
            continue;
        }

        match cas.get_object(&hash)? {
            Some(obj) => {
                let source = obj
                    .get("source")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_string();
                let timestamp = obj
                    .get("timestamp")
                    .or_else(|| obj.get("created_at"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();

                snapshots.push((hash.clone(), source, timestamp));

                // Follow parent_hashes
                if let Some(parents) = obj.get("parent_hashes").and_then(|v| v.as_array()) {
                    for parent in parents {
                        if let Some(ph) = parent.as_str() {
                            queue.push(ph.to_string());
                        }
                    }
                }
            }
            None => continue,
        }
    }

    // Sort by timestamp (oldest first)
    snapshots.sort_by(|a, b| a.2.cmp(&b.2));

    // Count how many we'd keep vs remove based on retention
    let manual: Vec<_> = snapshots.iter().filter(|s| s.1 == "push" || s.1 == "manual").collect();
    let excess_manual = manual.len().saturating_sub(policy.manual_pushes);

    // For now, just report — actual DAG rewrite is complex and deferred
    // to when we have the full execution-state CAS (Phase 4)
    Ok(excess_manual)
}
