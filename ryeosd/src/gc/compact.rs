//! Phase 2: History compaction.
//!
//! Walks the project snapshot DAG via `parent_hashes`, applies the
//! retention policy, and rewrites the DAG using a memoized recursive
//! pass to correctly handle transitive removals and propagate
//! rewritten hashes upward.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

use crate::cas::CasStore;
use crate::refs::RefStore;

use super::RetentionPolicy;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CompactionResult {
    pub projects_scanned: usize,
    pub snapshots_removed: usize,
    pub snapshots_rewritten: usize,
}

struct SnapshotInfo {
    hash: String,
    source: String,
    created_at: String,
    parent_hashes: Vec<String>,
}

pub fn run_compact(
    cas: &CasStore,
    refs: &RefStore,
    cas_root: &Path,
    policy: &RetentionPolicy,
    dry_run: bool,
) -> Result<CompactionResult> {
    let mut result = CompactionResult::default();

    let refs_root = cas_root;
    if !refs_root.is_dir() {
        return Ok(result);
    }

    let pinned: HashSet<String> = refs
        .list_pins()?
        .into_iter()
        .filter_map(|name| refs.read_pin(&name).ok().flatten())
        .collect();

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

        let principal_fp = principal_entry.file_name().to_string_lossy().to_string();

        for project_entry in std::fs::read_dir(&projects_dir)? {
            let project_entry = project_entry?;
            let project_dir = project_entry.path();
            let head_file = project_dir.join("head");
            if !head_file.is_file() {
                continue;
            }

            result.projects_scanned += 1;

            let head_hash = std::fs::read_to_string(&head_file)?.trim().to_string();

            // Read the real project_path from meta.json, not the hashed dir name.
            let meta_file = project_dir.join("meta.json");
            let real_project_path = if meta_file.is_file() {
                let meta: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(&meta_file)?)?;
                meta.get("project_path")
                    .and_then(|v| v.as_str())
                    .map(String::from)
            } else {
                None
            };

            let real_project_path = match real_project_path {
                Some(p) => p,
                None => {
                    tracing::warn!(
                        principal = %principal_fp,
                        dir = %project_dir.display(),
                        "skipping compaction: no meta.json with project_path"
                    );
                    continue;
                }
            };

            match compact_project_dag(
                cas,
                refs,
                &head_hash,
                &principal_fp,
                &real_project_path,
                policy,
                &pinned,
                dry_run,
            ) {
                Ok((removed, rewritten)) => {
                    result.snapshots_removed += removed;
                    result.snapshots_rewritten += rewritten;
                }
                Err(e) => {
                    tracing::warn!(
                        principal = %principal_fp,
                        project = %real_project_path,
                        error = %e,
                        "compaction failed for project, skipping"
                    );
                }
            }
        }
    }

    tracing::info!(
        projects = result.projects_scanned,
        removed = result.snapshots_removed,
        rewritten = result.snapshots_rewritten,
        dry_run,
        "compact phase complete"
    );

    Ok(result)
}

fn compact_project_dag(
    cas: &CasStore,
    refs: &RefStore,
    head_hash: &str,
    principal_fp: &str,
    project_path: &str,
    policy: &RetentionPolicy,
    pinned: &HashSet<String>,
    dry_run: bool,
) -> Result<(usize, usize)> {
    // ── Collect all snapshots in the DAG ────────────────────────────

    let mut snapshots: Vec<SnapshotInfo> = Vec::new();
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
                let created_at = obj
                    .get("created_at")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let parent_hashes: Vec<String> = obj
                    .get("parent_hashes")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();

                for parent in &parent_hashes {
                    queue.push(parent.clone());
                }

                snapshots.push(SnapshotInfo {
                    hash: hash.clone(),
                    source,
                    created_at,
                    parent_hashes,
                });
            }
            None => continue,
        }
    }

    // ── Determine keep/remove sets ─────────────────────────────────

    let snapshot_map: HashMap<&str, &SnapshotInfo> =
        snapshots.iter().map(|s| (s.hash.as_str(), s)).collect();

    let mut keep: HashSet<&str> = HashSet::new();
    keep.insert(head_hash);

    let mut manual: Vec<&SnapshotInfo> = snapshots
        .iter()
        .filter(|s| s.source == "push" || s.source == "manual")
        .collect();
    manual.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    for s in manual.iter().take(policy.manual_pushes) {
        keep.insert(&s.hash);
    }

    let mut daily: Vec<&SnapshotInfo> = snapshots
        .iter()
        .filter(|s| {
            s.source == "checkpoint_daily"
                || (s.source != "push"
                    && s.source != "manual"
                    && s.source != "checkpoint_weekly")
        })
        .collect();
    daily.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    for s in daily.iter().take(policy.daily_checkpoints) {
        keep.insert(&s.hash);
    }

    let mut weekly: Vec<&SnapshotInfo> = snapshots
        .iter()
        .filter(|s| s.source == "checkpoint_weekly")
        .collect();
    weekly.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    for s in weekly.iter().take(policy.weekly_checkpoints) {
        keep.insert(&s.hash);
    }

    for hash in pinned.iter() {
        keep.insert(hash.as_str());
    }

    let remove_set: HashSet<&str> = snapshots
        .iter()
        .filter(|s| !keep.contains(s.hash.as_str()))
        .map(|s| s.hash.as_str())
        .collect();

    if remove_set.is_empty() {
        return Ok((0, 0));
    }

    if dry_run {
        tracing::info!(
            head = head_hash,
            candidates = remove_set.len(),
            "dry run: would remove snapshots"
        );
        return Ok((remove_set.len(), 0));
    }

    // ── Memoized recursive rewrite ─────────────────────────────────
    //
    // For removed nodes: resolve_parents() recursively lifts through
    // chains of removed ancestors to find the surviving ones.
    //
    // For kept nodes: rewrite_kept() recursively rewrites parents
    // bottom-up, memoizing results so each node is processed once.
    // If a kept node's parent list doesn't change, its hash is reused.

    let mut resolved_cache: HashMap<String, Vec<String>> = HashMap::new();
    let mut rewrite_cache: HashMap<String, String> = HashMap::new();
    let mut total_rewritten = 0usize;

    // Recursively resolve the surviving ancestors of a removed node.
    fn resolve_parents<'a>(
        hash: &str,
        snapshot_map: &HashMap<&str, &SnapshotInfo>,
        remove_set: &HashSet<&str>,
        cache: &'a mut HashMap<String, Vec<String>>,
    ) -> Vec<String> {
        if let Some(cached) = cache.get(hash) {
            return cached.clone();
        }

        let info = match snapshot_map.get(hash) {
            Some(i) => i,
            None => return vec![],
        };

        let mut surviving = Vec::new();
        for parent in &info.parent_hashes {
            if remove_set.contains(parent.as_str()) {
                // Parent is also removed — recurse to find its surviving ancestors
                let grandparents = resolve_parents(parent, snapshot_map, remove_set, cache);
                surviving.extend(grandparents);
            } else {
                surviving.push(parent.clone());
            }
        }

        surviving.sort();
        surviving.dedup();
        cache.insert(hash.to_string(), surviving.clone());
        surviving
    }

    // Recursively rewrite a kept node, ensuring all parents are rewritten first.
    fn rewrite_kept(
        hash: &str,
        cas: &CasStore,
        snapshot_map: &HashMap<&str, &SnapshotInfo>,
        remove_set: &HashSet<&str>,
        resolved_cache: &mut HashMap<String, Vec<String>>,
        rewrite_cache: &mut HashMap<String, String>,
        rewritten_count: &mut usize,
    ) -> Result<String> {
        if let Some(cached) = rewrite_cache.get(hash) {
            return Ok(cached.clone());
        }

        let info = match snapshot_map.get(hash) {
            Some(i) => i,
            None => {
                // Not in our DAG — return as-is (e.g. a root with no object)
                rewrite_cache.insert(hash.to_string(), hash.to_string());
                return Ok(hash.to_string());
            }
        };

        // Build new parent list: for each parent, either rewrite it (if kept)
        // or resolve its surviving ancestors (if removed).
        let mut new_parents = Vec::new();
        let mut changed = false;

        for parent in &info.parent_hashes {
            if remove_set.contains(parent.as_str()) {
                // Removed parent — lift to surviving ancestors
                let surviving =
                    resolve_parents(parent, snapshot_map, remove_set, resolved_cache);
                new_parents.extend(surviving);
                changed = true;
            } else {
                // Kept parent — recursively rewrite it first
                let rewritten = rewrite_kept(
                    parent,
                    cas,
                    snapshot_map,
                    remove_set,
                    resolved_cache,
                    rewrite_cache,
                    rewritten_count,
                )?;
                if rewritten != *parent {
                    changed = true;
                }
                new_parents.push(rewritten);
            }
        }

        new_parents.sort();
        new_parents.dedup();

        if !changed {
            // No parents changed — hash stays the same
            rewrite_cache.insert(hash.to_string(), hash.to_string());
            return Ok(hash.to_string());
        }

        // Parents changed — store a new snapshot object with updated parent_hashes
        let obj = cas.get_object(hash)?
            .ok_or_else(|| anyhow::anyhow!("snapshot {hash} disappeared during compaction"))?;

        let mut new_obj = obj.clone();
        new_obj
            .as_object_mut()
            .unwrap()
            .insert("parent_hashes".to_string(), serde_json::json!(new_parents));

        let new_hash = cas.store_object(&new_obj)?;

        tracing::info!(
            old_hash = hash,
            new_hash = %new_hash,
            "rewrote snapshot parent_hashes"
        );

        *rewritten_count += 1;
        rewrite_cache.insert(hash.to_string(), new_hash.clone());
        Ok(new_hash)
    }

    // Rewrite the entire DAG starting from HEAD
    let new_head = rewrite_kept(
        head_hash,
        cas,
        &snapshot_map,
        &remove_set,
        &mut resolved_cache,
        &mut rewrite_cache,
        &mut total_rewritten,
    )?;

    // Update HEAD if it changed
    if new_head != head_hash {
        if !refs.advance_project_ref(principal_fp, project_path, &new_head, Some(head_hash))? {
            bail!("HEAD moved during compaction, aborting");
        }
        tracing::info!(
            old_head = head_hash,
            new_head = %new_head,
            project = project_path,
            "updated HEAD after compaction"
        );
    }

    Ok((remove_set.len(), total_rewritten))
}
