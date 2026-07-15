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

use crate::objects::ProjectSnapshot;
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
    skip(cas_root, refs_root, trust_store, signer, policy),
    fields(dry_run = dry_run)
)]
pub fn compact_projects(
    cas_root: &Path,
    refs_root: &Path,
    trust_store: &refs::TrustStore,
    signer: &dyn Signer,
    policy: &RetentionPolicy,
    dry_run: bool,
) -> Result<CompactionResult> {
    let runtime_path = cas_root
        .parent()
        .ok_or_else(|| anyhow::anyhow!("CAS root has no runtime-state parent"))?;
    if refs_root.parent() != Some(runtime_path) {
        anyhow::bail!("CAS and refs roots do not share one runtime-state parent");
    }
    let runtime = lillux::PinnedDirectory::open(runtime_path)?
        .ok_or_else(|| anyhow::anyhow!("runtime-state directory is absent"))?;
    let cas_directory = runtime
        .open_child_directory(std::ffi::OsStr::new("objects"))?
        .ok_or_else(|| anyhow::anyhow!("CAS root is absent"))?;
    let refs_directory = runtime
        .open_child_directory(std::ffi::OsStr::new("refs"))?
        .ok_or_else(|| anyhow::anyhow!("refs root is absent"))?;
    compact_projects_pinned(
        &lillux::CasStore::from_pinned_root(cas_directory),
        &refs_directory,
        trust_store,
        signer,
        policy,
        dry_run,
    )
}

/// Compact project history from one descriptor-bound refs/CAS authority.
pub(crate) fn compact_projects_pinned(
    cas: &lillux::CasStore,
    refs_directory: &lillux::PinnedDirectory,
    trust_store: &refs::TrustStore,
    signer: &dyn Signer,
    policy: &RetentionPolicy,
    dry_run: bool,
) -> Result<CompactionResult> {
    crate::signer::ensure_signer_trusted(signer, trust_store)
        .context("GC compaction signer is not admitted by authoritative head trust")?;
    let mut result = CompactionResult::default();
    for head in refs::list_verified_project_head_refs_in_directory(refs_directory, trust_store)? {
        result.projects_scanned += 1;
        let project_result = compact_single_project(
            cas,
            refs_directory,
            &head.principal_key,
            &head.project_hash,
            trust_store,
            signer,
            policy,
            dry_run,
        )
        .with_context(|| {
            format!(
                "compaction failed for principal/project {}/{}",
                head.principal_key, head.project_hash
            )
        })?;
        result.snapshots_removed += project_result.snapshots_removed;
        result.snapshots_rewritten += project_result.snapshots_rewritten;
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
    cas: &lillux::CasStore,
    refs_directory: &lillux::PinnedDirectory,
    principal_key: &str,
    project_hash: &str,
    trust_store: &refs::TrustStore,
    signer: &dyn Signer,
    policy: &RetentionPolicy,
    dry_run: bool,
) -> Result<ProjectCompactionResult> {
    // Hold the same cross-process lock as every other project HEAD mutator
    // across the verified read, DAG rewrite, and compare-and-swap advance.
    // The CAS check alone cannot prevent two processes from both advancing
    // the same observed HEAD without this shared critical section.
    let _head_lock = if dry_run {
        refs::ProjectHeadLock::acquire_existing_in_refs_directory(
            refs_directory,
            principal_key,
            project_hash,
        )?
    } else {
        refs::ProjectHeadLock::acquire_in_refs_directory(
            refs_directory,
            principal_key,
            project_hash,
        )?
    };
    let head = refs::read_verified_project_head_ref_in_directory(
        refs_directory,
        principal_key,
        project_hash,
        trust_store,
    )?;
    let head_hash = head.map(|head| head.target_hash).ok_or_else(|| {
        anyhow::anyhow!(
            "no project head for principal/project {}/{}",
            principal_key,
            project_hash
        )
    })?;

    // Step 1: Walk the full DAG and collect all snapshots with metadata.
    // walk_dag is strict: returns error on missing/corrupt objects.
    let mut all_snapshots: Vec<SnapshotInfo> = Vec::new();
    let mut visited: HashSet<String> = HashSet::new();
    walk_dag_with_cas(cas, &head_hash, &mut all_snapshots, &mut visited)?;
    ensure_snapshot_dag_is_acyclic(&all_snapshots)?;

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
        let limit = if is_manual {
            policy.manual_pushes
        } else {
            policy.auto_snapshots
        };
        let count = if is_manual {
            &mut manual_count
        } else {
            &mut auto_count
        };

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
            kept_topo.len(),
            keep.len(),
            project_hash
        );
    }

    let mut hash_remap: HashMap<String, String> = HashMap::new();
    let mut snapshots_rewritten = 0usize;

    for snap_hash in &kept_topo {
        let snap = all_snapshots.iter().find(|s| s.hash == *snap_hash).unwrap();

        // Resolve parents: removed → lift to surviving ancestors,
        // remapped → use new hash
        let mut new_parents: Vec<String> =
            resolve_surviving_parents(&snap.parent_hashes, &keep, &all_snapshots)
                .into_iter()
                .map(|p| hash_remap.get(&p).cloned().unwrap_or(p))
                .collect();
        let mut seen_parents = HashSet::with_capacity(new_parents.len());
        new_parents.retain(|parent| seen_parents.insert(parent.clone()));

        if new_parents == snap.parent_hashes {
            continue; // No change needed
        }

        // Rewrite only the already strictly loaded schema-4 object. This
        // prevents compaction from copying unvalidated fields into a newly
        // authoritative object. Dry-run performs this exact rewrite and hash
        // propagation too, so descendant rewrites and the would-be final HEAD
        // are classified identically; it suppresses publication only.
        let mut rewritten = snap.snapshot.clone();
        rewritten.parent_hashes = new_parents;
        let value = rewritten.to_value();
        ProjectSnapshot::from_value(&value)
            .context("validate rewritten project snapshot before CAS publication")?;
        let canonical = lillux::canonical_json(&value)
            .with_context(|| format!("failed to canonicalize rewritten snapshot {snap_hash}"))?;
        let new_hash = lillux::sha256_hex(canonical.as_bytes());

        if new_hash != *snap_hash {
            if !dry_run {
                let stored = cas
                    .put_object(&value)
                    .with_context(|| format!("failed to write rewritten snapshot {new_hash}"))?;
                if stored.hash != new_hash {
                    anyhow::bail!(
                        "rewritten snapshot hash changed during CAS publication: expected {new_hash}, got {}",
                        stored.hash
                    );
                }
            }

            hash_remap.insert(snap_hash.clone(), new_hash);
            snapshots_rewritten += 1;
        }
    }

    // Step 5: Determine final HEAD hash and update ref if changed
    let new_head_hash = hash_remap
        .get(&head_hash)
        .cloned()
        .unwrap_or(head_hash.clone());

    if new_head_hash != head_hash && !dry_run {
        refs::advance_verified_project_head_ref_in_directory(
            refs_directory,
            principal_key,
            project_hash,
            &new_head_hash,
            &head_hash,
            signer,
            trust_store,
            &_head_lock,
        )?;
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
    snapshot: ProjectSnapshot,
}

fn walk_dag_with_cas(
    cas: &lillux::CasStore,
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

        let snapshot = load_project_snapshot_with_cas(cas, &hash)?;
        let parent_hashes = snapshot.parent_hashes.clone();

        snapshots.push(SnapshotInfo {
            hash: hash.clone(),
            source: snapshot.source.clone(),
            parent_hashes: parent_hashes.clone(),
            created_at: snapshot.created_at.clone(),
            snapshot,
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

/// Load one exact current-schema project snapshot from CAS. Compaction is an
/// authoritative writer, so it accepts neither a body stored under the wrong
/// requested hash nor a semantically equivalent non-canonical encoding.
#[cfg(test)]
fn load_project_snapshot(cas_root: &Path, requested_hash: &str) -> Result<ProjectSnapshot> {
    let cas = lillux::CasStore::new(cas_root.to_path_buf());
    load_project_snapshot_with_cas(&cas, requested_hash)
}

fn load_project_snapshot_with_cas(
    cas: &lillux::CasStore,
    requested_hash: &str,
) -> Result<ProjectSnapshot> {
    let value = cas
        .get_object(requested_hash)
        .with_context(|| format!("compaction: load snapshot {requested_hash}"))?
        .ok_or_else(|| anyhow::anyhow!("compaction: snapshot {requested_hash} is missing"))?;
    let snapshot = ProjectSnapshot::from_value(&value)
        .with_context(|| format!("compaction: invalid snapshot {requested_hash}"))?;
    if snapshot
        .parent_hashes
        .iter()
        .any(|parent| parent == requested_hash)
    {
        anyhow::bail!("compaction: project snapshot {requested_hash} references itself");
    }
    Ok(snapshot)
}

/// Validate the complete discovered DAG before applying retention. A cycle
/// entirely inside the would-be removed set is still corruption and must not
/// be hidden by retention selection.
fn ensure_snapshot_dag_is_acyclic(snapshots: &[SnapshotInfo]) -> Result<()> {
    let mut in_degree: HashMap<&str, usize> = snapshots
        .iter()
        .map(|snapshot| (snapshot.hash.as_str(), 0))
        .collect();
    let mut children: HashMap<&str, Vec<&str>> = HashMap::new();
    for snapshot in snapshots {
        for parent in &snapshot.parent_hashes {
            if !in_degree.contains_key(parent.as_str()) {
                anyhow::bail!(
                    "compaction: snapshot {} references undiscovered parent {}",
                    snapshot.hash,
                    parent
                );
            }
            let Some(degree) = in_degree.get_mut(snapshot.hash.as_str()) else {
                unreachable!("snapshot map was built from the same slice");
            };
            *degree = degree
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("compaction: snapshot in-degree overflow"))?;
            children
                .entry(parent.as_str())
                .or_default()
                .push(snapshot.hash.as_str());
        }
    }

    let mut queue: VecDeque<&str> = in_degree
        .iter()
        .filter_map(|(hash, degree)| (*degree == 0).then_some(*hash))
        .collect();
    let mut visited = 0usize;
    while let Some(hash) = queue.pop_front() {
        visited += 1;
        if let Some(descendants) = children.get(hash) {
            for child in descendants {
                let degree = in_degree
                    .get_mut(*child)
                    .expect("child was discovered from the snapshot set");
                *degree -= 1;
                if *degree == 0 {
                    queue.push_back(*child);
                }
            }
        }
    }
    if visited != snapshots.len() {
        anyhow::bail!("compaction: project snapshot DAG contains a cycle");
    }
    Ok(())
}

/// Sort kept snapshots so parents come before children.
/// Returns hashes in order: roots first, HEAD last.
/// Uses Kahn's algorithm for deterministic topological sort.
fn topological_sort_kept(all_snapshots: &[SnapshotInfo], keep: &HashSet<String>) -> Vec<String> {
    // Build adjacency: for each kept snapshot, its kept parents
    let snap_map: HashMap<&str, &SnapshotInfo> =
        all_snapshots.iter().map(|s| (s.hash.as_str(), s)).collect();

    let mut in_degree: HashMap<String, usize> = HashMap::new();
    let mut children: HashMap<String, Vec<String>> = HashMap::new();

    for hash in keep {
        in_degree.entry(hash.clone()).or_insert(0);
        if let Some(snap) = snap_map.get(hash.as_str()) {
            for parent in &snap.parent_hashes {
                // Only count kept parents (removed ones are resolved separately)
                if keep.contains(parent) {
                    *in_degree.entry(hash.clone()).or_insert(0) += 1;
                    children
                        .entry(parent.clone())
                        .or_default()
                        .push(hash.clone());
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signer::TestSigner;
    use std::fs;

    fn test_signer_and_trust() -> (TestSigner, refs::TrustStore) {
        let signer = TestSigner::default();
        let mut trust_store = refs::TrustStore::new();
        trust_store.insert(signer.fingerprint().to_string(), signer.verifying_key());
        (signer, trust_store)
    }

    fn make_hash(suffix: &str) -> String {
        // Use SHA-256 to ensure unique hashes per suffix
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut hasher = DefaultHasher::new();
        suffix.hash(&mut hasher);
        // Combine with byte sum for additional uniqueness
        let byte_sum: u64 = suffix
            .as_bytes()
            .iter()
            .fold(0u64, |a, &b| a.wrapping_add(b as u64));
        let h = hasher.finish().wrapping_add(byte_sum);
        format!("{h:064x}")
    }

    fn write_object(cas_root: &Path, value: &serde_json::Value) -> String {
        let canonical = lillux::canonical_json(value).unwrap();
        let hash = lillux::sha256_hex(canonical.as_bytes());
        let path = lillux::shard_path(cas_root, "objects", &hash, ".json");
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        lillux::atomic_write(&path, canonical.as_bytes()).unwrap();
        hash
    }

    fn write_project_head(
        refs_root: &Path,
        principal_key: &str,
        project_hash: &str,
        target_hash: &str,
        signer: &dyn Signer,
        trust_store: &refs::TrustStore,
    ) {
        let project_lock =
            refs::ProjectHeadLock::acquire(refs_root, principal_key, project_hash).unwrap();
        refs::write_verified_project_head_ref(
            refs_root,
            principal_key,
            project_hash,
            target_hash,
            signer,
            trust_store,
            &project_lock,
        )
        .unwrap();
    }

    fn write_project_snapshot(
        cas_root: &Path,
        label: &str,
        source: &str,
        parents: &[&str],
    ) -> String {
        write_project_snapshot_with_time(cas_root, label, source, parents, "2026-04-23T00:00:00Z")
    }

    fn write_project_snapshot_with_time(
        cas_root: &Path,
        label: &str,
        source: &str,
        parents: &[&str],
        created_at: &str,
    ) -> String {
        let snapshot = ProjectSnapshot {
            project_manifest_hash: make_hash(&format!("manifest-{label}")),
            user_manifest_hash: None,
            message: None,
            project_sync_scope: crate::project_sync::ProjectSyncScope::FullProject,
            parent_hashes: parents.iter().map(|parent| (*parent).to_string()).collect(),
            created_at: created_at.to_string(),
            source: source.to_string(),
        };
        write_object(cas_root, &snapshot.to_value())
    }

    fn snapshot_info(hash: &str, source: &str, parents: &[&str], created_at: &str) -> SnapshotInfo {
        let snapshot = ProjectSnapshot {
            project_manifest_hash: make_hash(&format!("manifest-{hash}")),
            user_manifest_hash: None,
            message: None,
            project_sync_scope: crate::project_sync::ProjectSyncScope::FullProject,
            parent_hashes: parents.iter().map(|parent| (*parent).to_string()).collect(),
            created_at: created_at.to_string(),
            source: source.to_string(),
        };
        SnapshotInfo {
            hash: hash.to_string(),
            source: snapshot.source.clone(),
            parent_hashes: snapshot.parent_hashes.clone(),
            created_at: snapshot.created_at.clone(),
            snapshot,
        }
    }

    fn write_raw_object(cas_root: &Path, requested_hash: &str, bytes: &[u8]) {
        let path = lillux::shard_path(cas_root, "objects", requested_hash, ".json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        lillux::atomic_write(&path, bytes).unwrap();
    }

    #[test]
    fn strict_snapshot_loader_rejects_wrong_hash_noncanonical_and_old_schema() {
        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("objects");
        let snapshot = ProjectSnapshot {
            project_manifest_hash: make_hash("strict-manifest"),
            user_manifest_hash: None,
            message: None,
            project_sync_scope: crate::project_sync::ProjectSyncScope::FullProject,
            parent_hashes: Vec::new(),
            created_at: "2026-07-14T00:00:00Z".to_string(),
            source: "manual_push".to_string(),
        };
        let value = snapshot.to_value();
        let canonical = lillux::canonical_json(&value).unwrap();
        let actual_hash = lillux::sha256_hex(canonical.as_bytes());
        write_raw_object(&cas_root, &actual_hash, canonical.as_bytes());
        load_project_snapshot(&cas_root, &actual_hash).unwrap();

        let wrong_hash = make_hash("wrong-location");
        write_raw_object(&cas_root, &wrong_hash, canonical.as_bytes());
        assert!(load_project_snapshot(&cas_root, &wrong_hash).is_err());

        let pretty = serde_json::to_vec_pretty(&value).unwrap();
        let pretty_hash = lillux::sha256_hex(&pretty);
        write_raw_object(&cas_root, &pretty_hash, &pretty);
        assert!(load_project_snapshot(&cas_root, &pretty_hash).is_err());

        let mut old_schema = value;
        old_schema["schema"] = serde_json::json!(ProjectSnapshot::SCHEMA - 1);
        let old_canonical = lillux::canonical_json(&old_schema).unwrap();
        let old_hash = lillux::sha256_hex(old_canonical.as_bytes());
        write_raw_object(&cas_root, &old_hash, old_canonical.as_bytes());
        assert!(load_project_snapshot(&cas_root, &old_hash).is_err());
    }

    #[test]
    fn strict_snapshot_loader_rejects_noncanonical_timestamp_and_full_dag_cycle() {
        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("objects");
        let mut value = ProjectSnapshot {
            project_manifest_hash: make_hash("time-manifest"),
            user_manifest_hash: None,
            message: None,
            project_sync_scope: crate::project_sync::ProjectSyncScope::FullProject,
            parent_hashes: Vec::new(),
            created_at: "2026-07-14T00:00:00Z".to_string(),
            source: "manual_push".to_string(),
        }
        .to_value();
        value["created_at"] = serde_json::json!("2026-07-14T00:00:00+00:00");
        let canonical = lillux::canonical_json(&value).unwrap();
        let hash = lillux::sha256_hex(canonical.as_bytes());
        write_raw_object(&cas_root, &hash, canonical.as_bytes());
        assert!(load_project_snapshot(&cas_root, &hash).is_err());

        let cyclic = vec![
            snapshot_info("a", "manual", &["b"], "2026-07-14T00:00:00Z"),
            snapshot_info("b", "manual", &["a"], "2026-07-14T00:00:01Z"),
        ];
        assert!(ensure_snapshot_dag_is_acyclic(&cyclic).is_err());
    }

    #[test]
    fn compact_removes_old_auto_snapshots() {
        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("objects");
        let refs_root = tmp.path().join("refs");
        fs::create_dir_all(&cas_root).unwrap();
        fs::create_dir_all(&refs_root).unwrap();
        let (signer, trust_store) = test_signer_and_trust();

        // Build a chain: snap5 -> snap4 -> snap3 -> snap2 -> snap1
        // All auto, keep only 2
        let snap1_hash = write_project_snapshot(&cas_root, "snap1", "fold_back", &[]);
        let snap2_hash = write_project_snapshot(&cas_root, "snap2", "fold_back", &[&snap1_hash]);
        let snap3_hash = write_project_snapshot(&cas_root, "snap3", "fold_back", &[&snap2_hash]);
        let snap4_hash = write_project_snapshot(&cas_root, "snap4", "fold_back", &[&snap3_hash]);
        let snap5_hash = write_project_snapshot(&cas_root, "snap5", "fold_back", &[&snap4_hash]);

        // HEAD points to snap5
        write_project_head(
            &refs_root,
            "fp:test-principal",
            "test-project",
            &snap5_hash,
            &signer,
            &trust_store,
        );

        let policy = RetentionPolicy {
            manual_pushes: 10,
            auto_snapshots: 2, // Keep HEAD (snap5) + snap4, snap3 only
        };

        let result =
            compact_projects(&cas_root, &refs_root, &trust_store, &signer, &policy, true).unwrap();

        assert_eq!(result.projects_scanned, 1);
        assert_eq!(result.snapshots_removed, 2); // snap1, snap2 removed; HEAD(snap5) + snap4, snap3 kept
        assert_eq!(
            result.snapshots_rewritten, 3,
            "dry-run must propagate snap3's rewritten hash through snap4 and HEAD"
        );
    }

    #[test]
    fn compact_keeps_manual_pushes() {
        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("objects");
        let refs_root = tmp.path().join("refs");
        fs::create_dir_all(&cas_root).unwrap();
        fs::create_dir_all(&refs_root).unwrap();
        let (signer, trust_store) = test_signer_and_trust();

        // 3 manual pushes: push3 -> push2 -> push1
        let push1_hash = write_project_snapshot(&cas_root, "push1", "push", &[]);
        let push2_hash = write_project_snapshot(&cas_root, "push2", "push", &[&push1_hash]);
        let push3_hash = write_project_snapshot(&cas_root, "push3", "push", &[&push2_hash]);

        write_project_head(
            &refs_root,
            "fp:test-principal",
            "test-proj",
            &push3_hash,
            &signer,
            &trust_store,
        );

        let policy = RetentionPolicy {
            manual_pushes: 10,
            auto_snapshots: 2,
        };

        let result =
            compact_projects(&cas_root, &refs_root, &trust_store, &signer, &policy, true).unwrap();
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
        let (signer, trust_store) = test_signer_and_trust();

        // 5 auto snapshots, keep 0 auto (only HEAD)
        let mut hashes = Vec::new();
        for i in 1..=5 {
            let parents: Vec<&str> = hashes.last().map(String::as_str).into_iter().collect();
            hashes.push(write_project_snapshot(
                &cas_root,
                &format!("auto{i}"),
                "fold_back",
                &parents,
            ));
        }

        write_project_head(
            &refs_root,
            "fp:test-principal",
            "test-proj",
            &hashes[4],
            &signer,
            &trust_store,
        );

        let policy = RetentionPolicy {
            manual_pushes: 0,
            auto_snapshots: 0, // Keep nothing beyond HEAD
        };

        let result =
            compact_projects(&cas_root, &refs_root, &trust_store, &signer, &policy, true).unwrap();
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
        let (signer, trust_store) = test_signer_and_trust();

        let result = compact_projects(
            &cas_root,
            &refs_root,
            &trust_store,
            &signer,
            &RetentionPolicy {
                manual_pushes: 10,
                auto_snapshots: 30,
            },
            false,
        )
        .unwrap();
        assert_eq!(result.projects_scanned, 0);
        assert_eq!(result.snapshots_removed, 0);
    }

    #[test]
    fn resolve_surviving_parents_basic() {
        let snap1 = snapshot_info("h1", "push", &[], "2026-01-01T00:00:00Z");
        let snap2 = snapshot_info("h2", "push", &["h1"], "2026-01-02T00:00:00Z");
        let snap3 = snapshot_info("h3", "auto", &["h2"], "2026-01-03T00:00:00Z");
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
        let (signer, trust_store) = test_signer_and_trust();

        // Build a linear chain: snap20 -> snap19 -> ... -> snap1
        let count = 20;
        let mut hashes = Vec::new();
        for i in 1..=count {
            let parents: Vec<&str> = hashes.last().map(String::as_str).into_iter().collect();
            hashes.push(write_project_snapshot(
                &cas_root,
                &format!("snap-{i:03}"),
                "fold_back",
                &parents,
            ));
        }

        // HEAD = snap20 (last element)
        let head_hash = &hashes[count - 1];
        write_project_head(
            &refs_root,
            "fp:test-principal",
            "scale-proj",
            head_hash,
            &signer,
            &trust_store,
        );

        // Keep HEAD + 4 auto = 5 total, remove 15
        let policy = RetentionPolicy {
            manual_pushes: 10,
            auto_snapshots: 4,
        };

        let result =
            compact_projects(&cas_root, &refs_root, &trust_store, &signer, &policy, true).unwrap();
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
        let (signer, trust_store) = test_signer_and_trust();

        // Chain: s5 -> s4 -> s3 -> s2 -> s1 (all auto)
        let s1 = write_project_snapshot(&cas_root, "i1", "fold_back", &[]);
        let s2 = write_project_snapshot(&cas_root, "i2", "fold_back", &[&s1]);
        let s3 = write_project_snapshot(&cas_root, "i3", "fold_back", &[&s2]);
        let s4 = write_project_snapshot(&cas_root, "i4", "fold_back", &[&s3]);
        let s5 = write_project_snapshot(&cas_root, "i5", "fold_back", &[&s4]);

        write_project_head(
            &refs_root,
            "fp:test-principal",
            "integrity-proj",
            &s5,
            &signer,
            &trust_store,
        );

        // Keep only HEAD + 1 auto = 2 total. Should remove s1, s2, s3.
        // s4 should rewrite its parent from s3 → (resolved through removed to root).
        let policy = RetentionPolicy {
            manual_pushes: 10,
            auto_snapshots: 1,
        };

        let result =
            compact_projects(&cas_root, &refs_root, &trust_store, &signer, &policy, true).unwrap();
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
        let (signer, trust_store) = test_signer_and_trust();

        // Build the merge DAG
        // E is the root (merge base)
        let e_hash = write_project_snapshot_with_time(
            &cas_root,
            "E",
            "fold_back",
            &[],
            "2026-04-20T00:00:00Z",
        );
        // C and D are children of E
        let c_hash = write_project_snapshot_with_time(
            &cas_root,
            "C",
            "fold_back",
            &[&e_hash],
            "2026-04-21T00:00:00Z",
        );
        let d_hash = write_project_snapshot_with_time(
            &cas_root,
            "D",
            "fold_back",
            &[&e_hash],
            "2026-04-21T00:00:00Z",
        );
        // A is child of C, B is child of D
        let a_hash = write_project_snapshot_with_time(
            &cas_root,
            "A",
            "fold_back",
            &[&c_hash],
            "2026-04-22T00:00:00Z",
        );
        let b_hash = write_project_snapshot_with_time(
            &cas_root,
            "B",
            "fold_back",
            &[&d_hash],
            "2026-04-22T00:00:00Z",
        );
        // HEAD has parents A and B
        let head_hash = write_project_snapshot_with_time(
            &cas_root,
            "HEAD",
            "fold_back",
            &[&a_hash, &b_hash],
            "2026-04-23T00:00:00Z",
        );

        write_project_head(
            &refs_root,
            "fp:test-principal",
            "merge-proj",
            &head_hash,
            &signer,
            &trust_store,
        );

        // Keep HEAD + 2 auto snapshots.
        // By created_at (newest first): HEAD(23rd), A(22nd), B(22nd), C(21st), D(21st), E(20th)
        // HEAD kept (always), then auto_snapshots=2: A and B kept (newest).
        // C, D, E removed.
        let policy = RetentionPolicy {
            manual_pushes: 0,
            auto_snapshots: 2,
        };

        let result =
            compact_projects(&cas_root, &refs_root, &trust_store, &signer, &policy, false).unwrap();

        assert_eq!(result.snapshots_removed, 3, "should remove C, D, E");
        assert!(
            result.snapshots_rewritten >= 2,
            "A and B should both be rewritten (C→E, D→E)"
        );

        // Verify A and B now point to E (not C/D)
        // Read the actual rewritten objects from CAS
        let new_head = refs::read_verified_project_head_ref(
            &refs_root,
            "fp:test-principal",
            "merge-proj",
            &trust_store,
        )
        .unwrap()
        .unwrap()
        .target_hash;
        let head_obj: serde_json::Value = {
            let path = lillux::shard_path(&cas_root, "objects", &new_head, ".json");
            let content = std::fs::read_to_string(&path).unwrap();
            serde_json::from_str(&content).unwrap()
        };
        let head_parents: Vec<String> = head_obj["parent_hashes"]
            .as_array()
            .unwrap()
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
                .as_array()
                .unwrap()
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
        let (signer, trust_store) = test_signer_and_trust();

        // Linear chain but timestamps are out of DAG order:
        //   head → mid_old → mid_new → old_root
        // DAG order:  head → mid_old → mid_new → old_root
        // Time order: mid_new(23rd) > head(22nd) > old_root(21st) > mid_old(20th)
        let old_root_hash = write_project_snapshot_with_time(
            &cas_root,
            "root",
            "fold_back",
            &[],
            "2026-04-21T00:00:00Z",
        );
        let mid_new_hash = write_project_snapshot_with_time(
            &cas_root,
            "mid-new",
            "fold_back",
            &[&old_root_hash],
            "2026-04-23T00:00:00Z",
        );
        let mid_old_hash = write_project_snapshot_with_time(
            &cas_root,
            "mid-old",
            "fold_back",
            &[&mid_new_hash],
            "2026-04-20T00:00:00Z",
        );
        let head_hash = write_project_snapshot_with_time(
            &cas_root,
            "head",
            "fold_back",
            &[&mid_old_hash],
            "2026-04-22T00:00:00Z",
        );

        write_project_head(
            &refs_root,
            "fp:test-principal",
            "time-proj",
            &head_hash,
            &signer,
            &trust_store,
        );

        // Keep HEAD + 1 auto. By created_at newest: mid_new(23rd), head(22nd), old_root(21st), mid_old(20th)
        // HEAD always kept. Then auto_snapshots=1: mid_new (newest non-HEAD).
        // Removed: old_root and mid_old.
        let policy = RetentionPolicy {
            manual_pushes: 0,
            auto_snapshots: 1,
        };

        let result =
            compact_projects(&cas_root, &refs_root, &trust_store, &signer, &policy, true).unwrap();

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
        let (signer, trust_store) = test_signer_and_trust();

        // HEAD points to a snapshot that references a nonexistent parent
        let missing_parent = make_hash("nonexistent");
        let head_hash =
            write_project_snapshot(&cas_root, "orphan-head", "fold_back", &[&missing_parent]);

        write_project_head(
            &refs_root,
            "fp:test-principal",
            "orphan-proj",
            &head_hash,
            &signer,
            &trust_store,
        );

        let policy = RetentionPolicy {
            manual_pushes: 10,
            auto_snapshots: 30,
        };

        // compact_projects propagates per-project errors (fail-hard).
        let result = compact_projects(&cas_root, &refs_root, &trust_store, &signer, &policy, false);
        assert!(
            result.is_err(),
            "compaction should fail on missing snapshot"
        );
        let err_msg = format!("{:#}", result.unwrap_err());
        assert!(
            err_msg.contains("orphan-proj"),
            "error should mention project hash, got: {}",
            err_msg
        );
    }
}
