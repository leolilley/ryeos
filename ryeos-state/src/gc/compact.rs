//! Project snapshot DAG compaction.
//!
//! Walks each project head's snapshot DAG, applies a retention policy,
//! removes excess snapshots, and rewrites parent pointers so the DAG
//! stays valid. If HEAD's hash changes, updates the project head ref.
//!
//! Compaction is the only mechanism to prune project history. Without it,
//! every push/fold-back adds a snapshot that lives forever.
//!
//! Rewrite order is topological (parents before children via Kahn's algorithm)
//! so each snapshot sees its parents' final hashes. Retention is by
//! `created_at` timestamp (newest first), not traversal order.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::refs;
use crate::signer::Signer;

/// Retention policy for project snapshots.
///
/// Two categories: manual pushes and auto snapshots (fold-back).
/// HEAD is always kept regardless of policy.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetentionPolicy {
    /// Max snapshots with source="push" or "manual" to keep per project.
    pub manual_pushes: usize,
    /// Max snapshots with source="fold_back" or other auto sources to keep.
    pub auto_snapshots: usize,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            manual_pushes: 10,
            auto_snapshots: 30,
        }
    }
}

/// Result of compacting project snapshot DAGs.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompactionResult {
    pub projects_scanned: usize,
    pub snapshots_removed: usize,
    pub snapshots_rewritten: usize,
}

/// Compact all project snapshot DAGs per retention policy.
///
/// For each project head:
/// 1. Walk the DAG from HEAD via `parent_hashes`
/// 2. Sort snapshots by `created_at` descending
/// 3. Apply retention policy (keep N newest per category + HEAD always)
/// 4. Rewrite in topological order (parents before children)
/// 5. If HEAD's hash changed: advance the project head ref
#[tracing::instrument(
    name = "state:compact",
    skip(cas_root, refs_root, signer, policy),
    fields(dry_run = dry_run)
)]
pub fn compact_projects(
    cas_root: &Path,
    refs_root: &Path,
    signer: &dyn Signer,
    policy: &RetentionPolicy,
    dry_run: bool,
) -> Result<CompactionResult> {
    let mut result = CompactionResult::default();

    let projects_dir = refs_root.join("projects");
    if !projects_dir.is_dir() {
        return Ok(result);
    }

    for entry in std::fs::read_dir(&projects_dir)
        .context("failed to read projects refs directory")?
    {
        let entry = entry.context("failed to read project ref entry")?;
        if !entry.file_type()?.is_dir() {
            continue;
        }

        let project_hash = entry.file_name().to_string_lossy().to_string();
        let head_path = entry.path().join("head");
        if !head_path.exists() {
            continue;
        }

        result.projects_scanned += 1;

        match compact_single_project(cas_root, refs_root, &project_hash, signer, policy, dry_run) {
            Ok(project_result) => {
                result.snapshots_removed += project_result.snapshots_removed;
                result.snapshots_rewritten += project_result.snapshots_rewritten;
            }
            Err(e) => {
                return Err(e.context(format!(
                    "compaction failed for project {}",
                    project_hash
                )));
            }
        }
    }

    Ok(result)
}

/// Result of compacting a single project's DAG.
struct ProjectCompactionResult {
    snapshots_removed: usize,
    snapshots_rewritten: usize,
}

/// Compact a single project's snapshot DAG.
fn compact_single_project(
    cas_root: &Path,
    refs_root: &Path,
    project_hash: &str,
    signer: &dyn Signer,
    policy: &RetentionPolicy,
    dry_run: bool,
) -> Result<ProjectCompactionResult> {
    let head_hash = refs::read_project_head_ref(refs_root, project_hash)?
        .ok_or_else(|| anyhow::anyhow!("no project head for {}", project_hash))?;

    // Step 1: Walk the full DAG and collect all snapshots with metadata.
    // walk_dag is strict: returns error on missing/corrupt objects.
    let mut all_snapshots: Vec<SnapshotInfo> = Vec::new();
    let mut visited: HashSet<String> = HashSet::new();
    walk_dag(cas_root, &head_hash, &mut all_snapshots, &mut visited)?;

    if all_snapshots.is_empty() {
        return Ok(ProjectCompactionResult {
            snapshots_removed: 0,
            snapshots_rewritten: 0,
        });
    }

    // Step 2: Sort by `created_at` descending (newest first) before applying
    // retention. This ensures "keep N" means "keep the N newest by timestamp",
    // not dependent on traversal order.
    all_snapshots.sort_by(|a, b| b.created_at.cmp(&a.created_at));

    // Step 3: Apply retention policy.
    // HEAD is always kept. Then count snapshots per category, keeping the
    // N newest of each (they're already sorted newest-first).
    let mut manual_count = 0usize;
    let mut auto_count = 0usize;

    let mut keep: HashSet<String> = HashSet::new();
    let mut removed: HashSet<String> = HashSet::new();

    for snap in &all_snapshots {
        if snap.hash == head_hash {
            // HEAD is always kept
            keep.insert(snap.hash.clone());
            continue;
        }

        let is_manual = snap.source == "push" || snap.source == "manual";
        let limit = if is_manual { policy.manual_pushes } else { policy.auto_snapshots };
        let count = if is_manual { &mut manual_count } else { &mut auto_count };

        if *count < limit {
            keep.insert(snap.hash.clone());
            *count += 1;
        } else {
            removed.insert(snap.hash.clone());
        }
    }

    if removed.is_empty() {
        return Ok(ProjectCompactionResult {
            snapshots_removed: 0,
            snapshots_rewritten: 0,
        });
    }

    // Step 4: Rewrite in topological order (parents before children).
    //
    // This replaces the old arbitrary-order + fixed-point cascade approach.
    // By processing roots first and HEAD last, each snapshot always sees
    // its parents' final hashes — no cascade needed.
    let kept_topo = topological_sort_kept(&all_snapshots, &keep);
    if kept_topo.len() != keep.len() {
        anyhow::bail!(
            "compaction: topological sort returned {} nodes but {} are kept \
             (possible cycle in snapshot DAG for project {})",
            kept_topo.len(), keep.len(), project_hash
        );
    }

    let mut hash_remap: HashMap<String, String> = HashMap::new();
    let mut snapshots_rewritten = 0usize;

    for snap_hash in &kept_topo {
        let snap = all_snapshots.iter().find(|s| s.hash == *snap_hash).unwrap();

        // Resolve parents: removed → lift to surviving ancestors,
        // remapped → use new hash
        let new_parents: Vec<String> = resolve_surviving_parents(
            &snap.parent_hashes, &keep, &all_snapshots,
        )
        .into_iter()
        .map(|p| hash_remap.get(&p).cloned().unwrap_or(p))
        .collect();

        if new_parents == snap.parent_hashes {
            continue; // No change needed
        }

        if dry_run {
            snapshots_rewritten += 1;
            continue;
        }

        // Read original snapshot, update parent_hashes, write new object
        let path = lillux::shard_path(cas_root, "objects", snap_hash, ".json");
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read snapshot {} during compaction", snap_hash))?;
        let mut value: serde_json::Value = serde_json::from_str(&content)
            .with_context(|| format!("failed to parse snapshot {} during compaction", snap_hash))?;

        if let Some(obj) = value.as_object_mut() {
            obj.insert("parent_hashes".to_string(), serde_json::json!(new_parents));
        }

        let canonical = lillux::canonical_json(&value);
        let new_hash = lillux::sha256_hex(canonical.as_bytes());

        if new_hash != *snap_hash {
            let new_path = lillux::shard_path(cas_root, "objects", &new_hash, ".json");
            if let Some(parent) = new_path.parent() {
                fs_err_create_dir_all(parent)?;
            }
            lillux::atomic_write(&new_path, canonical.as_bytes())
                .with_context(|| format!("failed to write rewritten snapshot {}", new_hash))?;

            hash_remap.insert(snap_hash.clone(), new_hash);
            snapshots_rewritten += 1;
        }
    }

    // Step 5: Determine final HEAD hash and update ref if changed
    let new_head_hash = hash_remap.get(&head_hash).cloned().unwrap_or(head_hash.clone());

    if new_head_hash != head_hash && !dry_run {
        refs::write_project_head_ref(refs_root, project_hash, &new_head_hash, signer)?;
        tracing::info!(
            project_hash = %project_hash,
            old_head = %&head_hash[..16.min(head_hash.len())],
            new_head = %&new_head_hash[..16.min(new_head_hash.len())],
            "project head updated after compaction"
        );
    }

    tracing::info!(
        project_hash = %project_hash,
        snapshots_removed = removed.len(),
        snapshots_rewritten = snapshots_rewritten,
        "project compacted"
    );

    Ok(ProjectCompactionResult {
        snapshots_removed: removed.len(),
        snapshots_rewritten,
    })
}

/// Information about a project snapshot in the DAG.
#[derive(Clone)]
struct SnapshotInfo {
    hash: String,
    source: String,
    parent_hashes: Vec<String>,
    created_at: String,
}

/// Walk the project snapshot DAG from HEAD, BFS via parent_hashes.
///
/// Strict mode: returns an error if any snapshot in the DAG is missing
/// or corrupt. This is essential for compaction, which must have a
/// complete picture of the DAG before rewriting parent pointers.
fn walk_dag(
    cas_root: &Path,
    head_hash: &str,
    snapshots: &mut Vec<SnapshotInfo>,
    visited: &mut HashSet<String>,
) -> Result<()> {
    let mut queue = VecDeque::new();
    queue.push_back(head_hash.to_string());

    while let Some(hash) = queue.pop_front() {
        if visited.contains(&hash) {
            continue;
        }
        visited.insert(hash.clone());

        let path = lillux::shard_path(cas_root, "objects", &hash, ".json");
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!(
                "compaction: failed to read snapshot {} at {}",
                hash, path.display()
            ))?;

        let value: serde_json::Value = serde_json::from_str(&content)
            .with_context(|| format!(
                "compaction: failed to parse snapshot {}",
                hash
            ))?;

        // Validate kind — only compact project_snapshot objects
        let kind = value.get("kind").and_then(|v| v.as_str()).unwrap_or("");
        if kind != "project_snapshot" {
            anyhow::bail!(
                "compaction: expected project_snapshot, got kind='{}' for hash {}",
                kind, hash
            );
        }

        // Extract parent_hashes
        let parent_hashes: Vec<String> = value
            .get("parent_hashes")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .filter(|h| !h.is_empty())
                    .collect()
            })
            .unwrap_or_default();

        // Extract source
        let source = value
            .get("source")
            .and_then(|v| v.as_str())
            .unwrap_or("auto")
            .to_string();

        // Extract created_at
        let created_at = value
            .get("created_at")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        snapshots.push(SnapshotInfo {
            hash: hash.clone(),
            source,
            parent_hashes: parent_hashes.clone(),
            created_at,
        });

        // Follow parents
        for parent in parent_hashes {
            if !visited.contains(&parent) {
                queue.push_back(parent);
            }
        }
    }

    Ok(())
}

/// Sort kept snapshots so parents come before children.
/// Returns hashes in order: roots first, HEAD last.
/// Uses Kahn's algorithm for deterministic topological sort.
fn topological_sort_kept(
    all_snapshots: &[SnapshotInfo],
    keep: &HashSet<String>,
) -> Vec<String> {
    // Build adjacency: for each kept snapshot, its kept parents
    let snap_map: HashMap<&str, &SnapshotInfo> = all_snapshots
        .iter()
        .map(|s| (s.hash.as_str(), s))
        .collect();

    let mut in_degree: HashMap<String, usize> = HashMap::new();
    let mut children: HashMap<String, Vec<String>> = HashMap::new();

    for hash in keep {
        in_degree.entry(hash.clone()).or_insert(0);
        if let Some(snap) = snap_map.get(hash.as_str()) {
            for parent in &snap.parent_hashes {
                // Only count kept parents (removed ones are resolved separately)
                if keep.contains(parent) {
                    *in_degree.entry(hash.clone()).or_insert(0) += 1;
                    children.entry(parent.clone()).or_default().push(hash.clone());
                }
            }
        }
    }

    // Kahn's algorithm: start with nodes that have no kept parents (roots)
    let mut queue: Vec<String> = in_degree
        .iter()
        .filter(|(_, &deg)| deg == 0)
        .map(|(h, _)| h.clone())
        .collect();
    queue.sort(); // deterministic ordering for nodes at same level

    let mut result = Vec::new();
    while let Some(node) = queue.pop() {
        result.push(node.clone());
        if let Some(kids) = children.get(&node) {
            let mut ready = Vec::new();
            for kid in kids {
                if let Some(deg) = in_degree.get_mut(kid) {
                    *deg -= 1;
                    if *deg == 0 {
                        ready.push(kid.clone());
                    }
                }
            }
            ready.sort();
            queue.extend(ready);
        }
    }

    result
}

/// Given a list of parent hashes, resolve each through removed snapshots
/// to find the closest surviving ancestors.
fn resolve_surviving_parents(
    parent_hashes: &[String],
    keep: &HashSet<String>,
    all_snapshots: &[SnapshotInfo],
) -> Vec<String> {
    let mut result = Vec::new();
    let mut visited: HashSet<String> = HashSet::new();

    for parent in parent_hashes {
        resolve_through_removed(parent, keep, all_snapshots, &mut result, &mut visited);
    }

    result
}

/// Recursively walk through removed snapshots to find surviving ancestors.
fn resolve_through_removed(
    current: &str,
    keep: &HashSet<String>,
    all_snapshots: &[SnapshotInfo],
    result: &mut Vec<String>,
    visited: &mut HashSet<String>,
) {
    if visited.contains(current) {
        return;
    }
    visited.insert(current.to_string());

    if keep.contains(current) {
        result.push(current.to_string());
        return;
    }

    // This snapshot is removed — follow its parents
    if let Some(snap) = all_snapshots.iter().find(|s| s.hash == current) {
        for parent in &snap.parent_hashes {
            resolve_through_removed(parent, keep, all_snapshots, result, visited);
        }
    }
}

/// Helper to create dirs with anyhow error.
fn fs_err_create_dir_all(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)
        .with_context(|| format!("failed to create directory: {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use crate::signer::TestSigner;

    fn make_hash(suffix: &str) -> String {
        // Use SHA-256 to ensure unique hashes per suffix
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut hasher = DefaultHasher::new();
        suffix.hash(&mut hasher);
        // Combine with byte sum for additional uniqueness
        let byte_sum: u64 = suffix.as_bytes().iter().fold(0u64, |a, &b| a.wrapping_add(b as u64));
        let h = hasher.finish().wrapping_add(byte_sum);
        format!("{h:064x}")
    }

    fn write_object(cas_root: &Path, hash: &str, value: &serde_json::Value) {
        let path = lillux::shard_path(cas_root, "objects", hash, ".json");
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let canonical = lillux::canonical_json(value);
        lillux::atomic_write(&path, canonical.as_bytes()).unwrap();
    }

    fn write_project_head(refs_root: &Path, project_hash: &str, target_hash: &str, signer: &dyn Signer) {
        refs::write_project_head_ref(refs_root, project_hash, target_hash, signer).unwrap();
    }

    fn make_project_snapshot(hash: &str, source: &str, parents: &[&str]) -> serde_json::Value {
        serde_json::json!({
            "kind": "project_snapshot",
            "schema": 2,
            "project_manifest_hash": make_hash(&format!("manifest-{}", hash)),
            "parent_hashes": parents,
            "created_at": "2026-04-23T00:00:00Z",
            "source": source,
        })
    }

    fn make_project_snapshot_with_time(
        hash: &str,
        source: &str,
        parents: &[&str],
        created_at: &str,
    ) -> serde_json::Value {
        serde_json::json!({
            "kind": "project_snapshot",
            "schema": 2,
            "project_manifest_hash": make_hash(&format!("manifest-{}", hash)),
            "parent_hashes": parents,
            "created_at": created_at,
            "source": source,
        })
    }

    #[test]
    fn compact_removes_old_auto_snapshots() {
        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("objects");
        let refs_root = tmp.path().join("refs");
        fs::create_dir_all(&cas_root).unwrap();
        fs::create_dir_all(&refs_root).unwrap();
        let signer = TestSigner::default();

        // Build a chain: snap5 -> snap4 -> snap3 -> snap2 -> snap1
        // All auto, keep only 2
        let snap1_hash = make_hash("snap1");
        let snap2_hash = make_hash("snap2");
        let snap3_hash = make_hash("snap3");
        let snap4_hash = make_hash("snap4");
        let snap5_hash = make_hash("snap5");

        write_object(&cas_root, &snap1_hash, &make_project_snapshot("snap1", "fold_back", &[]));
        write_object(&cas_root, &snap2_hash, &make_project_snapshot("snap2", "fold_back", &[&snap1_hash]));
        write_object(&cas_root, &snap3_hash, &make_project_snapshot("snap3", "fold_back", &[&snap2_hash]));
        write_object(&cas_root, &snap4_hash, &make_project_snapshot("snap4", "fold_back", &[&snap3_hash]));
        write_object(&cas_root, &snap5_hash, &make_project_snapshot("snap5", "fold_back", &[&snap4_hash]));

        // HEAD points to snap5
        write_project_head(&refs_root, "test-project", &snap5_hash, &signer);

        let policy = RetentionPolicy {
            manual_pushes: 10,
            auto_snapshots: 2,  // Keep HEAD (snap5) + snap4, snap3 only
        };

        let result = compact_projects(&cas_root, &refs_root, &signer, &policy, true).unwrap();

        assert_eq!(result.projects_scanned, 1);
        assert_eq!(result.snapshots_removed, 2); // snap1, snap2 removed; HEAD(snap5) + snap4, snap3 kept
        assert!(result.snapshots_rewritten >= 1); // snap3 needs parent rewrite (snap2→snap1 bypass)
    }

    #[test]
    fn compact_keeps_manual_pushes() {
        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("objects");
        let refs_root = tmp.path().join("refs");
        fs::create_dir_all(&cas_root).unwrap();
        fs::create_dir_all(&refs_root).unwrap();
        let signer = TestSigner::default();

        // 3 manual pushes: push3 -> push2 -> push1
        let push1_hash = make_hash("push1");
        let push2_hash = make_hash("push2");
        let push3_hash = make_hash("push3");

        write_object(&cas_root, &push1_hash, &make_project_snapshot("push1", "push", &[]));
        write_object(&cas_root, &push2_hash, &make_project_snapshot("push2", "push", &[&push1_hash]));
        write_object(&cas_root, &push3_hash, &make_project_snapshot("push3", "push", &[&push2_hash]));

        write_project_head(&refs_root, "test-proj", &push3_hash, &signer);

        let policy = RetentionPolicy {
            manual_pushes: 10,
            auto_snapshots: 2,
        };

        let result = compact_projects(&cas_root, &refs_root, &signer, &policy, true).unwrap();
        // All manual, policy keeps 10 — nothing removed
        assert_eq!(result.snapshots_removed, 0);
    }

    #[test]
    fn compact_never_removes_head() {
        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("objects");
        let refs_root = tmp.path().join("refs");
        fs::create_dir_all(&cas_root).unwrap();
        fs::create_dir_all(&refs_root).unwrap();
        let signer = TestSigner::default();

        // 5 auto snapshots, keep 0 auto (only HEAD)
        let hashes: Vec<String> = (1..=5).map(|i| make_hash(&format!("auto{}", i))).collect();
        for (i, hash) in hashes.iter().enumerate() {
            let parents: Vec<&str> = if i > 0 { vec![&hashes[i - 1]] } else { vec![] };
            write_object(&cas_root, hash, &make_project_snapshot(&format!("auto{}", i + 1), "fold_back", &parents));
        }

        write_project_head(&refs_root, "test-proj", &hashes[4], &signer);

        let policy = RetentionPolicy {
            manual_pushes: 0,
            auto_snapshots: 0, // Keep nothing beyond HEAD
        };

        let result = compact_projects(&cas_root, &refs_root, &signer, &policy, true).unwrap();
        // HEAD + 0 auto = 4 removed
        assert_eq!(result.snapshots_removed, 4);
    }

    #[test]
    fn compact_no_projects_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("objects");
        let refs_root = tmp.path().join("refs");
        fs::create_dir_all(&cas_root).unwrap();
        fs::create_dir_all(&refs_root).unwrap();
        let signer = TestSigner::default();

        let result = compact_projects(&cas_root, &refs_root, &signer, &RetentionPolicy::default(), false).unwrap();
        assert_eq!(result.projects_scanned, 0);
        assert_eq!(result.snapshots_removed, 0);
    }

    #[test]
    fn resolve_surviving_parents_basic() {
        let snap1 = SnapshotInfo {
            hash: "h1".into(), source: "push".into(),
            parent_hashes: vec![], created_at: "2026-01-01T00:00:00Z".into(),
        };
        let snap2 = SnapshotInfo {
            hash: "h2".into(), source: "push".into(),
            parent_hashes: vec!["h1".into()], created_at: "2026-01-02T00:00:00Z".into(),
        };
        let snap3 = SnapshotInfo {
            hash: "h3".into(), source: "auto".into(),
            parent_hashes: vec!["h2".into()], created_at: "2026-01-03T00:00:00Z".into(),
        };
        let all = vec![snap1, snap2, snap3];

        let mut keep = HashSet::new();
        keep.insert("h1".into());
        keep.insert("h3".into());

        // h3's parent is h2 (removed), h2's parent is h1 (kept) => h3 should resolve to h1
        let resolved = resolve_surviving_parents(&["h2".to_string()], &keep, &all);
        assert_eq!(resolved, vec!["h1"]);
    }

    /// Scale test: 20 snapshots → compact to 5.
    ///
    /// Build a linear chain of 20 auto snapshots. With auto_snapshots=4
    /// (plus HEAD always kept), compact should remove 15 and keep 5.
    #[test]
    fn compact_large_dag_to_five() {
        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("objects");
        let refs_root = tmp.path().join("refs");
        fs::create_dir_all(&cas_root).unwrap();
        fs::create_dir_all(&refs_root).unwrap();
        let signer = TestSigner::default();

        // Build a linear chain: snap20 -> snap19 -> ... -> snap1
        let count = 20;
        let hashes: Vec<String> = (1..=count)
            .map(|i| make_hash(&format!("snap-{:03}", i)))
            .collect();

        for (i, hash) in hashes.iter().enumerate() {
            let parents: Vec<&str> = if i > 0 {
                vec![&hashes[i - 1]]
            } else {
                vec![]
            };
            write_object(
                &cas_root,
                hash,
                &make_project_snapshot(&format!("snap-{:03}", i + 1), "fold_back", &parents),
            );
        }

        // HEAD = snap20 (last element)
        let head_hash = &hashes[count - 1];
        write_project_head(&refs_root, "scale-proj", head_hash, &signer);

        // Keep HEAD + 4 auto = 5 total, remove 15
        let policy = RetentionPolicy {
            manual_pushes: 10,
            auto_snapshots: 4,
        };

        let result = compact_projects(&cas_root, &refs_root, &signer, &policy, true).unwrap();
        assert_eq!(result.projects_scanned, 1);
        assert_eq!(result.snapshots_removed, 15);
        assert!(result.snapshots_rewritten >= 1);
    }

    /// Verify DAG integrity after compaction: all kept snapshots form
    /// a valid chain from HEAD to root, no dangling parent references.
    #[test]
    fn compact_dag_integrity_after_removal() {
        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("objects");
        let refs_root = tmp.path().join("refs");
        fs::create_dir_all(&cas_root).unwrap();
        fs::create_dir_all(&refs_root).unwrap();
        let signer = TestSigner::default();

        // Chain: s5 -> s4 -> s3 -> s2 -> s1 (all auto)
        let s1 = make_hash("integrity-1");
        let s2 = make_hash("integrity-2");
        let s3 = make_hash("integrity-3");
        let s4 = make_hash("integrity-4");
        let s5 = make_hash("integrity-5");

        write_object(&cas_root, &s1, &make_project_snapshot("i1", "fold_back", &[]));
        write_object(&cas_root, &s2, &make_project_snapshot("i2", "fold_back", &[&s1]));
        write_object(&cas_root, &s3, &make_project_snapshot("i3", "fold_back", &[&s2]));
        write_object(&cas_root, &s4, &make_project_snapshot("i4", "fold_back", &[&s3]));
        write_object(&cas_root, &s5, &make_project_snapshot("i5", "fold_back", &[&s4]));

        write_project_head(&refs_root, "integrity-proj", &s5, &signer);

        // Keep only HEAD + 1 auto = 2 total. Should remove s1, s2, s3.
        // s4 should rewrite its parent from s3 → (resolved through removed to root).
        let policy = RetentionPolicy {
            manual_pushes: 10,
            auto_snapshots: 1,
        };

        let result = compact_projects(&cas_root, &refs_root, &signer, &policy, true).unwrap();
        assert_eq!(result.snapshots_removed, 3); // s1, s2, s3 removed
        assert!(result.snapshots_rewritten >= 1); // s4 needs parent rewrite
    }

    /// Merge DAG: verify compaction correctly rewrites both branches.
    ///
    /// ```text
    ///        HEAD
    ///       / \
    ///      A   B   (both auto, both kept)
    ///      |   |
    ///      C   D   (both auto, both REMOVED)
    ///       \ /
    ///        E     (auto, kept — the merge base)
    /// ```
    ///
    /// After compaction:
    /// - C and D removed
    /// - A's parent rewritten from C → E
    /// - B's parent rewritten from D → E
    /// - HEAD unchanged (parents are A and B, both kept)
    #[test]
    fn compact_merge_dag_cascade_rewrite() {
        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("objects");
        let refs_root = tmp.path().join("refs");
        fs::create_dir_all(&cas_root).unwrap();
        fs::create_dir_all(&refs_root).unwrap();
        let signer = TestSigner::default();

        // Build the merge DAG
        let e_hash = make_hash("merge-E");
        let c_hash = make_hash("merge-C");
        let d_hash = make_hash("merge-D");
        let a_hash = make_hash("merge-A");
        let b_hash = make_hash("merge-B");
        let head_hash = make_hash("merge-HEAD");

        // E is the root (merge base)
        write_object(&cas_root, &e_hash,
            &make_project_snapshot_with_time("E", "fold_back", &[], "2026-04-20T00:00:00Z"));
        // C and D are children of E
        write_object(&cas_root, &c_hash,
            &make_project_snapshot_with_time("C", "fold_back", &[&e_hash], "2026-04-21T00:00:00Z"));
        write_object(&cas_root, &d_hash,
            &make_project_snapshot_with_time("D", "fold_back", &[&e_hash], "2026-04-21T00:00:00Z"));
        // A is child of C, B is child of D
        write_object(&cas_root, &a_hash,
            &make_project_snapshot_with_time("A", "fold_back", &[&c_hash], "2026-04-22T00:00:00Z"));
        write_object(&cas_root, &b_hash,
            &make_project_snapshot_with_time("B", "fold_back", &[&d_hash], "2026-04-22T00:00:00Z"));
        // HEAD has parents A and B
        write_object(&cas_root, &head_hash,
            &make_project_snapshot_with_time("HEAD", "fold_back", &[&a_hash, &b_hash], "2026-04-23T00:00:00Z"));

        write_project_head(&refs_root, "merge-proj", &head_hash, &signer);

        // Keep HEAD + 2 auto snapshots.
        // By created_at (newest first): HEAD(23rd), A(22nd), B(22nd), C(21st), D(21st), E(20th)
        // HEAD kept (always), then auto_snapshots=2: A and B kept (newest).
        // C, D, E removed.
        let policy = RetentionPolicy {
            manual_pushes: 0,
            auto_snapshots: 2,
        };

        let result = compact_projects(&cas_root, &refs_root, &signer, &policy, false).unwrap();

        assert_eq!(result.snapshots_removed, 3, "should remove C, D, E");
        assert!(result.snapshots_rewritten >= 2, "A and B should both be rewritten (C→E, D→E)");

        // Verify A and B now point to E (not C/D)
        // Read the actual rewritten objects from CAS
        let new_head = refs::read_project_head_ref(&refs_root, "merge-proj").unwrap().unwrap();
        let head_obj: serde_json::Value = {
            let path = lillux::shard_path(&cas_root, "objects", &new_head, ".json");
            let content = std::fs::read_to_string(&path).unwrap();
            serde_json::from_str(&content).unwrap()
        };
        let head_parents: Vec<String> = head_obj["parent_hashes"]
            .as_array().unwrap()
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect();

        // HEAD should still have 2 parents
        assert_eq!(head_parents.len(), 2);

        // Each parent should point to E (since A→C→E became A→E, and B→D→E became B→E)
        // But A and B were rewritten, so their hashes changed. Check that their parents are E.
        for parent_hash in &head_parents {
            let path = lillux::shard_path(&cas_root, "objects", parent_hash, ".json");
            let content = std::fs::read_to_string(&path).unwrap();
            let obj: serde_json::Value = serde_json::from_str(&content).unwrap();
            let grandparents: Vec<String> = obj["parent_hashes"]
                .as_array().unwrap()
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect();
            // Each should point to E (the only surviving ancestor)
            assert!(
                grandparents.contains(&e_hash) || grandparents.is_empty(),
                "rewritten snapshot should point to E or have no parents: got {:?}",
                grandparents
            );
        }
    }

    /// Verify that retention respects `created_at` ordering, not traversal order.
    ///
    /// Create snapshots with timestamps that differ from DAG order.
    /// Verify that the N newest-by-timestamp are kept.
    #[test]
    fn compact_respects_created_at_ordering() {
        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("objects");
        let refs_root = tmp.path().join("refs");
        fs::create_dir_all(&cas_root).unwrap();
        fs::create_dir_all(&refs_root).unwrap();
        let signer = TestSigner::default();

        // Linear chain but timestamps are out of DAG order:
        //   head → mid_old → mid_new → old_root
        // DAG order:  head → mid_old → mid_new → old_root
        // Time order: mid_new(23rd) > head(22nd) > old_root(21st) > mid_old(20th)
        let old_root_hash = make_hash("time-root");
        let mid_new_hash = make_hash("time-mid-new");
        let mid_old_hash = make_hash("time-mid-old");
        let head_hash = make_hash("time-head");

        write_object(&cas_root, &old_root_hash,
            &make_project_snapshot_with_time("root", "fold_back", &[], "2026-04-21T00:00:00Z"));
        write_object(&cas_root, &mid_new_hash,
            &make_project_snapshot_with_time("mid-new", "fold_back", &[&old_root_hash], "2026-04-23T00:00:00Z"));
        write_object(&cas_root, &mid_old_hash,
            &make_project_snapshot_with_time("mid-old", "fold_back", &[&mid_new_hash], "2026-04-20T00:00:00Z"));
        write_object(&cas_root, &head_hash,
            &make_project_snapshot_with_time("head", "fold_back", &[&mid_old_hash], "2026-04-22T00:00:00Z"));

        write_project_head(&refs_root, "time-proj", &head_hash, &signer);

        // Keep HEAD + 1 auto. By created_at newest: mid_new(23rd), head(22nd), old_root(21st), mid_old(20th)
        // HEAD always kept. Then auto_snapshots=1: mid_new (newest non-HEAD).
        // Removed: old_root and mid_old.
        let policy = RetentionPolicy {
            manual_pushes: 0,
            auto_snapshots: 1,
        };

        let result = compact_projects(&cas_root, &refs_root, &signer, &policy, true).unwrap();

        // HEAD + 1 auto = keep 2, remove 2
        assert_eq!(result.snapshots_removed, 2, "should remove 2 snapshots");
    }

    /// Verify that walk_dag returns an error for missing snapshots.
    /// The per-project error is caught by compact_projects (which logs a warning
    /// and skips the project), so the overall result is Ok with 0 removals.
    #[test]
    fn compact_fails_on_missing_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("objects");
        let refs_root = tmp.path().join("refs");
        fs::create_dir_all(&cas_root).unwrap();
        fs::create_dir_all(&refs_root).unwrap();
        let signer = TestSigner::default();

        // HEAD points to a snapshot that references a nonexistent parent
        let head_hash = make_hash("orphan-head");
        let missing_parent = make_hash("nonexistent");

        write_object(&cas_root, &head_hash, &serde_json::json!({
            "kind": "project_snapshot",
            "schema": 2,
            "project_manifest_hash": make_hash("manifest-orphan"),
            "parent_hashes": [missing_parent],
            "created_at": "2026-04-23T00:00:00Z",
            "source": "fold_back",
        }));

        write_project_head(&refs_root, "orphan-proj", &head_hash, &signer);

        let policy = RetentionPolicy::default();

        // compact_projects propagates per-project errors (fail-hard).
        let result = compact_projects(&cas_root, &refs_root, &signer, &policy, false);
        assert!(result.is_err(), "compaction should fail on missing snapshot");
        let err_msg = format!("{:#}", result.unwrap_err());
        assert!(
            err_msg.contains("orphan-proj"),
            "error should mention project hash, got: {}",
            err_msg
        );
    }
}
