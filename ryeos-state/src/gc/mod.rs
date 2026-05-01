//! Garbage collection for CAS objects.
//!
//! Two-phase pipeline:
//! 1. **Compact** (opt-in, `--compact` flag): prune project snapshot DAGs
//!    per retention policy. Removes excess history, rewrites parent pointers.
//! 2. **Sweep** (always): mark-and-sweep unreachable CAS objects and blobs.
//!
//! Compact runs BEFORE sweep because compaction makes snapshots unreachable
//! (by removing them from the DAG). Sweep then deletes the orphaned objects.
//!
//! Submodules:
//! - `lock`: flock-based GC lock (prevents concurrent runs)
//! - `event_log`: JSONL append-only operational log
//! - `compact`: project snapshot DAG compaction with retention policy

pub mod compact;
pub mod event_log;
pub mod lock;

use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::reachability;

// Re-export key types
pub use compact::CompactionResult;
pub use compact::RetentionPolicy;
pub use event_log::GcEvent;
pub use lock::GcLock;

/// GC parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GcParams {
    /// Don't delete anything, just report.
    pub dry_run: bool,
    /// Compact project snapshot history before sweep.
    pub compact: bool,
    /// Retention policy for compaction (uses default if None).
    pub policy: Option<RetentionPolicy>,
}

/// Full GC result.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GcResult {
    pub roots_walked: usize,
    pub reachable_objects: usize,
    pub reachable_blobs: usize,
    pub deleted_objects: usize,
    pub deleted_blobs: usize,
    pub freed_bytes: u64,
    pub compaction: Option<CompactionResult>,
    pub duration_ms: u64,
}

/// Full GC pipeline: compact (optional) → sweep.
///
/// Compact runs first to make snapshot DAG orphans, then sweep collects
/// the final reachable set and deletes everything else.
///
/// `signer` is required only when `compact=true` (to update project head refs).
/// Sweep-only GC doesn't need signing authority.
pub fn run_gc(
    cas_root: &Path,
    refs_root: &Path,
    signer: Option<&dyn crate::Signer>,
    params: &GcParams,
) -> Result<GcResult> {
    let started = Instant::now();
    let mut result = GcResult::default();

    // Phase 1: Compact (opt-in)
    if params.compact {
        let compact_signer = signer
            .ok_or_else(|| anyhow::anyhow!("--compact requires a signer (use --key to provide one)"))?;

        let policy = params
            .policy
            .clone()
            .unwrap_or_default();

        tracing::info!(
            dry_run = params.dry_run,
            manual_pushes = policy.manual_pushes,
            auto_snapshots = policy.auto_snapshots,
            "starting project compaction"
        );

        let compaction = compact::compact_projects(
            cas_root,
            refs_root,
            compact_signer,
            &policy,
            params.dry_run,
        )?;

        tracing::info!(
            projects_scanned = compaction.projects_scanned,
            snapshots_removed = compaction.snapshots_removed,
            snapshots_rewritten = compaction.snapshots_rewritten,
            "compaction complete"
        );
        result.compaction = Some(compaction);
    }

    // Phase 2: Sweep (always)
    let reachable = reachability::collect_reachable(cas_root, refs_root)?;

    result.roots_walked = reachable.chain_root_ids.len() + reachable.project_hashes.len();
    result.reachable_objects = reachable.object_hashes.len();
    result.reachable_blobs = reachable.blob_hashes.len();

    sweep_sharded_dir(cas_root, "objects", ".json", &reachable.object_hashes, params.dry_run, &mut result)?;
    sweep_sharded_dir(cas_root, "blobs", "", &reachable.blob_hashes, params.dry_run, &mut result)?;

    result.duration_ms = started.elapsed().as_millis() as u64;

    tracing::info!(
        roots_walked = result.roots_walked,
        reachable_objects = result.reachable_objects,
        reachable_blobs = result.reachable_blobs,
        deleted_objects = result.deleted_objects,
        deleted_blobs = result.deleted_blobs,
        freed_bytes = result.freed_bytes,
        duration_ms = result.duration_ms,
        dry_run = params.dry_run,
        "GC complete"
    );

    Ok(result)
}

/// Sweep a sharded directory, deleting files not in the reachable set.
///
/// The shard layout is `namespace/ab/cd/hash{ext}` (two-level hex sharding).
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
            if !shard2.file_type()?.is_dir() {
                continue;
            }

            for file_entry in std::fs::read_dir(shard2.path())
                .with_context(|| format!("failed to read shard leaf directory"))?
            {
                let file_entry = file_entry.context("failed to read shard leaf entry")?;
                if !file_entry.file_type()?.is_file() {
                    continue;
                }

                let filename = file_entry.file_name();
                let filename_str = filename.to_string_lossy();
                let hash = if ext.is_empty() {
                    filename_str.to_string()
                } else if let Some(stripped) = filename_str.strip_suffix(ext) {
                    stripped.to_string()
                } else {
                    continue;
                };

                if !reachable.contains(&hash) {
                    let file_size = file_entry.metadata().map(|m| m.len()).unwrap_or(0);

                    if dry_run {
                        tracing::info!(
                            namespace = namespace,
                            hash = %&hash[..16.min(hash.len())],
                            size = file_size,
                            "would delete (dry run)"
                        );
                    } else {
                        std::fs::remove_file(file_entry.path())
                            .with_context(|| format!("failed to delete {}", file_entry.path().display()))?;
                    }

                    if namespace == "blobs" {
                        result.deleted_blobs += 1;
                    } else {
                        result.deleted_objects += 1;
                    }
                    result.freed_bytes += file_size;
                }
            }

            // Clean empty shard2 directories (bottom-up: leaf first)
            if !dry_run {
                let _ = remove_dir_if_empty(&shard2.path());
            }
        }

        // Clean empty shard1 directories (after shard2 dirs are cleaned)
        if !dry_run {
            let _ = remove_dir_if_empty(&shard1.path());
        }
    }

    Ok(())
}

/// Remove a directory only if it's empty. Ignores errors (best-effort).
fn remove_dir_if_empty(path: &std::path::Path) -> std::io::Result<()> {
    let mut entries = std::fs::read_dir(path)?;
    if entries.next().is_none() {
        std::fs::remove_dir(path)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gc_result_default() {
        let result = GcResult::default();
        assert_eq!(result.deleted_objects, 0);
        assert_eq!(result.reachable_blobs, 0);
    }

    #[test]
    fn retention_policy_default() {
        let policy = RetentionPolicy::default();
        assert_eq!(policy.manual_pushes, 10);
        assert_eq!(policy.auto_snapshots, 30);
    }

    #[test]
    fn gc_params_construction() {
        let params = GcParams {
            dry_run: true,
            compact: false,
            policy: None,
        };
        assert!(params.dry_run);
        assert!(!params.compact);
        assert!(params.policy.is_none());
    }

    /// Integration test: compact then sweep clears compacted victims.
    ///
    /// Creates a project snapshot chain, compacts it (removing some snapshots),
    /// then runs a full GC. The removed snapshots should be swept as unreachable.
    #[test]
    fn compact_then_sweep_cleans_victims() {
        use crate::signer::TestSigner;
        use crate::refs;
        use std::fs;

        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("objects");
        let refs_root = tmp.path().join("refs");
        fs::create_dir_all(&cas_root).unwrap();
        fs::create_dir_all(&refs_root).unwrap();
        let signer = TestSigner::default();

        fn make_hash(suffix: &str) -> String {
            use std::collections::hash_map::DefaultHasher;
            use std::hash::{Hash, Hasher};
            let mut hasher = DefaultHasher::new();
            suffix.hash(&mut hasher);
            let byte_sum: u64 = suffix
                .as_bytes()
                .iter()
                .fold(0u64, |a, &b| a.wrapping_add(b as u64));
            let h = hasher.finish().wrapping_add(byte_sum);
            format!("{h:064x}")
        }

        fn write_object(cas_root: &std::path::Path, hash: &str, value: &serde_json::Value) {
            let path = lillux::shard_path(cas_root, "objects", hash, ".json");
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            let canonical = lillux::canonical_json(value);
            lillux::atomic_write(&path, canonical.as_bytes()).unwrap();
        }

        // Write a chain: snap5 -> snap4 -> snap3 -> snap2 -> snap1 (all auto)
        let hashes: Vec<String> = (1..=5)
            .map(|i| make_hash(&format!("vict-snap{}", i)))
            .collect();

        for (i, hash) in hashes.iter().enumerate() {
            let parents: Vec<String> = if i > 0 {
                vec![hashes[i - 1].clone()]
            } else {
                vec![]
            };
            write_object(
                &cas_root,
                hash,
                &serde_json::json!({
                    "kind": "project_snapshot",
                    "schema": 2,
                    "project_manifest_hash": make_hash(&format!("m{}", i)),
                    "parent_hashes": parents,
                    "created_at": "2026-04-23T00:00:00Z",
                    "source": "fold_back",
                }),
            );
        }

        // Also write the manifests referenced by each snapshot so they're reachable
        for i in 0..5 {
            let manifest_hash = make_hash(&format!("m{}", i));
            write_object(
                &cas_root,
                &manifest_hash,
                &serde_json::json!({
                    "kind": "source_manifest",
                    "item_source_hashes": {},
                }),
            );
        }

        // Set HEAD to snap5
        refs::write_project_head_ref(&refs_root, "victim-proj", &hashes[4], &signer).unwrap();

        // Count objects before GC
        let count_before = count_objects(&cas_root);
        assert!(count_before >= 10); // 5 snapshots + 5 manifests

        // Verify compaction directly (dry run first to check logic)
        let policy = RetentionPolicy {
            manual_pushes: 10,
            auto_snapshots: 1,
        };
        let dry_compact = compact::compact_projects(
            &cas_root, &refs_root, &signer, &policy, true,
        ).unwrap();
        assert_eq!(dry_compact.projects_scanned, 1);
        assert_eq!(dry_compact.snapshots_removed, 3, "dry run should remove 3 snapshots");

        // Run GC with compact (keep HEAD + 1 auto = 2 snapshots, remove 3)
        let params = GcParams {
            dry_run: false,
            compact: true,
            policy: Some(policy),
        };

        let result = run_gc(&cas_root, &refs_root, Some(&signer), &params).unwrap();

        assert!(result.compaction.is_some());
        let compaction = result.compaction.unwrap();
        assert_eq!(compaction.snapshots_removed, 3);
        assert!(result.deleted_objects >= 3, "should have deleted at least 3 unreachable snapshots");

        let count_after = count_objects(&cas_root);
        assert!(
            count_after < count_before,
            "expected fewer objects after GC: before={}, after={}, deleted={}",
            count_before, count_after, result.deleted_objects
        );
    }

    /// Sweep-only GC (no compact) on empty CAS is a no-op.
    #[test]
    fn sweep_only_empty_cas() {
        use crate::signer::TestSigner;
        use std::fs;

        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("objects");
        let refs_root = tmp.path().join("refs");
        fs::create_dir_all(&cas_root).unwrap();
        fs::create_dir_all(&refs_root).unwrap();

        let signer = TestSigner::default();
        let params = GcParams {
            dry_run: false,
            compact: false,
            policy: None,
        };

        let result = run_gc(&cas_root, &refs_root, Some(&signer), &params).unwrap();
        assert_eq!(result.deleted_objects, 0);
        assert_eq!(result.deleted_blobs, 0);
        assert!(result.compaction.is_none());
    }

    fn count_objects(cas_root: &std::path::Path) -> usize {
        let objects_dir = cas_root.join("objects");
        if !objects_dir.is_dir() {
            return 0;
        }
        let mut count = 0;
        count_objects_recursive(&objects_dir, &mut count);
        count
    }

    fn count_objects_recursive(dir: &std::path::Path, count: &mut usize) {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                if entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false) {
                    count_objects_recursive(&entry.path(), count);
                } else if entry.file_type().map(|ft| ft.is_file()).unwrap_or(false) {
                    *count += 1;
                }
            }
        }
    }

    /// Shard-path contract test: verify that sweep finds files created by
    /// `lillux::shard_path`. This guards against shard depth mismatches
    /// (the prior regression where sweep was 2-level but layout was 3-level).
    #[test]
    fn sweep_finds_files_created_by_lillux_shard_path() {
        use std::collections::HashSet;
        use std::fs;

        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("cas");

        // Use valid 64-char hex hashes (as required by lillux::shard_path)
        let object_hashes: Vec<String> = vec![
            lillux::sha256_hex(b"test-object-1"),
            lillux::sha256_hex(b"test-object-2"),
            lillux::sha256_hex(b"test-object-3"),
        ];
        for hash in &object_hashes {
            let path = lillux::shard_path(&cas_root, "objects", hash, ".json");
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, b"{}").unwrap();
        }

        let blob_hash = lillux::sha256_hex(b"test-blob-1");
        let blob_path = lillux::shard_path(&cas_root, "blobs", &blob_hash, "");
        fs::create_dir_all(blob_path.parent().unwrap()).unwrap();
        fs::write(&blob_path, b"content").unwrap();

        // Run sweep with empty reachable set — everything should be deleted
        let reachable_objects: HashSet<String> = HashSet::new();
        let reachable_blobs: HashSet<String> = HashSet::new();
        let mut result = GcResult::default();

        sweep_sharded_dir(&cas_root, "objects", ".json", &reachable_objects, false, &mut result).unwrap();
        sweep_sharded_dir(&cas_root, "blobs", "", &reachable_blobs, false, &mut result).unwrap();

        assert_eq!(result.deleted_objects, 3, "sweep should find all 3 objects created by shard_path");
        assert_eq!(result.deleted_blobs, 1, "sweep should find the blob created by shard_path");

        // Verify files are actually gone
        for hash in &object_hashes {
            let path = lillux::shard_path(&cas_root, "objects", hash, ".json");
            assert!(!path.exists(), "object file should be deleted: {}", path.display());
        }
        assert!(!blob_path.exists(), "blob file should be deleted");
    }

    /// Verify that empty shard directories are cleaned up bottom-up after sweep.
    #[test]
    fn sweep_cleans_empty_shard_directories() {
        use std::collections::HashSet;
        use std::fs;

        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("cas");

        // Create a single object via shard_path
        let hash = lillux::sha256_hex(b"lonely-object");
        let path = lillux::shard_path(&cas_root, "objects", &hash, ".json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"{}").unwrap();

        // Verify shard dirs exist
        let shard2 = path.parent().unwrap();
        let shard1 = shard2.parent().unwrap();
        assert!(shard1.is_dir(), "shard1 dir should exist before sweep");
        assert!(shard2.is_dir(), "shard2 dir should exist before sweep");

        // Sweep with empty reachable — deletes the file and cleans dirs
        let reachable: HashSet<String> = HashSet::new();
        let mut result = GcResult::default();
        sweep_sharded_dir(&cas_root, "objects", ".json", &reachable, false, &mut result).unwrap();

        assert_eq!(result.deleted_objects, 1);
        assert!(!path.exists(), "file should be gone");
        assert!(!shard2.exists(), "shard2 dir should be cleaned up");
        assert!(!shard1.exists(), "shard1 dir should be cleaned up");
    }
}
