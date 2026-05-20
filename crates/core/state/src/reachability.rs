//! Shared reachability traversal for CAS-as-truth.
//!
//! Walks all signed heads (chains + projects) and collects every reachable
//! CAS object hash via BFS. Used by rebuild, verify, sync, and GC.

use std::collections::{HashSet, VecDeque};
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::Value;

/// Complete set of reachable hashes from all signed heads.
#[derive(Debug, Clone, Default)]
pub struct ReachableSet {
    /// All reachable JSON object hashes.
    pub object_hashes: HashSet<String>,
    /// All reachable blob hashes (raw binary content).
    pub blob_hashes: HashSet<String>,
    /// Chain root IDs that were walked.
    pub chain_root_ids: Vec<String>,
    /// Project hashes that were walked.
    pub project_hashes: Vec<String>,
}

/// Walk all signed heads (chains + projects) and collect every reachable
/// CAS hash via BFS.
///
/// Traversal rules:
///   signed chain head → chain_state → threads map (snapshot_hash, last_event_hash)
///     → thread_snapshot (base_project_snapshot_hash, result_project_snapshot_hash, last_event_hash)
///     → thread_event (prev_chain_event_hash, prev_thread_event_hash)
///   chain_state.prev_chain_state_hash → walk history
///
///   signed project head → project_snapshot (project_manifest_hash, user_manifest_hash, parent_hashes)
///     → source_manifest (item_source_hashes values)
///       → item_source (content_blob_hash) → blob
///   project_snapshot.parent_hashes → walk history
pub fn collect_reachable(
    cas_root: &Path,
    refs_root: &Path,
) -> Result<ReachableSet> {
    let mut set = ReachableSet::default();
    let mut queue: VecDeque<String> = VecDeque::new();

    // Seed from chain heads
    let chains_dir = refs_root.join("generic/chains");
    if chains_dir.is_dir() {
        for entry in std::fs::read_dir(&chains_dir)
            .context("failed to read chains refs directory")?
        {
            let entry = entry.context("failed to read chain ref entry")?;
            if entry.file_type()?.is_dir() {
                let chain_root_id = entry
                    .file_name()
                    .to_string_lossy()
                    .to_string();
                let head_path = entry.path().join("head");
                if head_path.exists() {
                    if let Some(target) = read_ref_target(&head_path)? {
                        queue.push_back(target);
                        set.chain_root_ids.push(chain_root_id);
                    }
                }
            }
        }
    }

    // Seed from project heads
    let projects_dir = refs_root.join("projects");
    if projects_dir.is_dir() {
        for entry in std::fs::read_dir(&projects_dir)
            .context("failed to read projects refs directory")?
        {
            let entry = entry.context("failed to read project ref entry")?;
            if entry.file_type()?.is_dir() {
                let project_hash = entry
                    .file_name()
                    .to_string_lossy()
                    .to_string();
                let head_path = entry.path().join("head");
                if head_path.exists() {
                    if let Some(target) = read_ref_target(&head_path)? {
                        queue.push_back(target);
                        set.project_hashes.push(project_hash);
                    }
                }
            }
        }
    }

    // BFS through the object graph
    while let Some(hash) = queue.pop_front() {
        if set.object_hashes.contains(&hash) {
            continue;
        }
        set.object_hashes.insert(hash.clone());

        let object_path = lillux::shard_path(cas_root, "objects", &hash, ".json");
        let content = match std::fs::read_to_string(&object_path) {
            Ok(c) => c,
            Err(_) => continue, // Object referenced but missing — skip
        };

        let value: Value = match serde_json::from_str(&content) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let kind = value
            .get("kind")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let children = extract_child_hashes(kind, &value);
        for child in children {
            if set.object_hashes.contains(&child) {
                continue;
            }
            queue.push_back(child);
        }

        // For item_source, extract blob hash
        if kind == "item_source" {
            if let Some(blob_hash) = value.get("content_blob_hash").and_then(|v| v.as_str()) {
                if !blob_hash.is_empty() && lillux::valid_hash(blob_hash) {
                    set.blob_hashes.insert(blob_hash.to_string());
                }
            }
        }
    }

    Ok(set)
}

/// Read the target hash from a signed ref file.
fn read_ref_target(path: &Path) -> Result<Option<String>> {
    let content = std::fs::read_to_string(path)?;
    let value: Value = serde_json::from_str(&content)?;
    let target = value
        .get("target_hash")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    Ok(target)
}

/// Extract child object hashes from a CAS object, dispatching on kind.
fn extract_child_hashes(kind: &str, value: &Value) -> Vec<String> {
    let mut children = Vec::new();

    match kind {
        "chain_state" => {
            // prev_chain_state_hash
            if let Some(hash) = value.get("prev_chain_state_hash").and_then(|v| v.as_str()) {
                if !hash.is_empty() {
                    children.push(hash.to_string());
                }
            }
            // threads map → snapshot_hash + last_event_hash per entry
            if let Some(threads) = value.get("threads").and_then(|v| v.as_object()) {
                for (_thread_id, entry) in threads {
                    if let Some(hash) = entry.get("snapshot_hash").and_then(|v| v.as_str()) {
                        if !hash.is_empty() {
                            children.push(hash.to_string());
                        }
                    }
                    if let Some(hash) = entry.get("last_event_hash").and_then(|v| v.as_str()) {
                        if !hash.is_empty() {
                            children.push(hash.to_string());
                        }
                    }
                }
            }
        }

        "thread_snapshot" => {
            // base_project_snapshot_hash
            if let Some(hash) = value
                .get("base_project_snapshot_hash")
                .and_then(|v| v.as_str())
            {
                if !hash.is_empty() {
                    children.push(hash.to_string());
                }
            }
            // result_project_snapshot_hash
            if let Some(hash) = value
                .get("result_project_snapshot_hash")
                .and_then(|v| v.as_str())
            {
                if !hash.is_empty() {
                    children.push(hash.to_string());
                }
            }
            // last_event_hash
            if let Some(hash) = value.get("last_event_hash").and_then(|v| v.as_str()) {
                if !hash.is_empty() {
                    children.push(hash.to_string());
                }
            }
        }

        "thread_event" => {
            // prev_chain_event_hash
            if let Some(hash) = value
                .get("prev_chain_event_hash")
                .and_then(|v| v.as_str())
            {
                if !hash.is_empty() {
                    children.push(hash.to_string());
                }
            }
            // prev_thread_event_hash
            if let Some(hash) = value
                .get("prev_thread_event_hash")
                .and_then(|v| v.as_str())
            {
                if !hash.is_empty() {
                    children.push(hash.to_string());
                }
            }
        }

        "project_snapshot" => {
            // project_manifest_hash
            if let Some(hash) = value
                .get("project_manifest_hash")
                .and_then(|v| v.as_str())
            {
                if !hash.is_empty() {
                    children.push(hash.to_string());
                }
            }
            // user_manifest_hash
            if let Some(hash) = value
                .get("user_manifest_hash")
                .and_then(|v| v.as_str())
            {
                if !hash.is_empty() {
                    children.push(hash.to_string());
                }
            }
            // parent_hashes (array)
            if let Some(parents) = value.get("parent_hashes").and_then(|v| v.as_array()) {
                for parent in parents {
                    if let Some(hash) = parent.as_str() {
                        if !hash.is_empty() {
                            children.push(hash.to_string());
                        }
                    }
                }
            }
        }

        "source_manifest" => {
            // item_source_hashes values
            if let Some(hashes) = value
                .get("item_source_hashes")
                .and_then(|v| v.as_object())
            {
                for (_path, hash_value) in hashes {
                    if let Some(hash) = hash_value.as_str() {
                        if !hash.is_empty() {
                            children.push(hash.to_string());
                        }
                    }
                }
            }
        }

        // item_source has no object children (only blob via content_blob_hash)
        // signed_ref, and unknown kinds: no children
        _ => {}
    }

    children
}

/// Collect reachable objects for a single chain only.
///
/// Useful for sync export of a specific chain.
pub fn collect_chain_reachable(
    cas_root: &Path,
    refs_root: &Path,
    chain_root_id: &str,
) -> Result<ReachableSet> {
    let mut set = ReachableSet::default();
    let mut queue: VecDeque<String> = VecDeque::new();

    let head_path = refs_root
        .join("generic/chains")
        .join(chain_root_id)
        .join("head");
    if let Some(target) = read_ref_target(&head_path)? {
        queue.push_back(target);
        set.chain_root_ids.push(chain_root_id.to_string());
    }

    while let Some(hash) = queue.pop_front() {
        if set.object_hashes.contains(&hash) {
            continue;
        }
        set.object_hashes.insert(hash.clone());

        let object_path = lillux::shard_path(cas_root, "objects", &hash, ".json");
        let content = match std::fs::read_to_string(&object_path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        let value: Value = match serde_json::from_str(&content) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let kind = value.get("kind").and_then(|v| v.as_str()).unwrap_or("");
        let children = extract_child_hashes(kind, &value);
        for child in children {
            if !set.object_hashes.contains(&child) {
                queue.push_back(child);
            }
        }

        if kind == "item_source" {
            if let Some(blob_hash) = value.get("content_blob_hash").and_then(|v| v.as_str()) {
                if !blob_hash.is_empty() && lillux::valid_hash(blob_hash) {
                    set.blob_hashes.insert(blob_hash.to_string());
                }
            }
        }
    }

    Ok(set)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn setup_cas_refs() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("state").join("objects");
        let refs_root = tmp.path().join("state").join("refs");
        (tmp, cas_root, refs_root)
    }

    fn write_object(cas_root: &Path, hash: &str, value: &Value) {
        let path = lillux::shard_path(cas_root, "objects", hash, ".json");
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let canonical = lillux::canonical_json(value);
        lillux::atomic_write(&path, canonical.as_bytes()).unwrap();
    }

    fn make_hash(suffix: &str) -> String {
        format!("{:064}", suffix.as_bytes().iter().fold(0u64, |a, &b| a.wrapping_add(b as u64)))
    }

    #[test]
    fn collect_reachable_empty_state() {
        let (_tmp, cas_root, refs_root) = setup_cas_refs();
        let set = collect_reachable(&cas_root, &refs_root).unwrap();
        assert!(set.object_hashes.is_empty());
        assert!(set.blob_hashes.is_empty());
        assert!(set.chain_root_ids.is_empty());
        assert!(set.project_hashes.is_empty());
    }

    #[test]
    fn collect_reachable_single_chain_head() {
        let (_tmp, cas_root, refs_root) = setup_cas_refs();

        // Create a minimal chain_state
        let cs_hash = make_hash("cs");
        let cs = serde_json::json!({
            "kind": "chain_state",
            "schema": 1,
            "chain_root_id": "T-root",
            "prev_chain_state_hash": null,
            "last_event_hash": null,
            "last_chain_seq": 0,
            "updated_at": "2026-04-22T00:00:00Z",
            "threads": {
                "T-root": {
                    "snapshot_hash": make_hash("snap"),
                    "last_event_hash": null,
                    "last_thread_seq": 0,
                    "status": "created"
                }
            }
        });

        // Create a minimal thread_snapshot
        let snap_hash = make_hash("snap");
        let snap = serde_json::json!({
            "kind": "thread_snapshot",
            "schema": 1,
            "thread_id": "T-root",
            "chain_root_id": "T-root",
            "status": "created",
            "kind_name": "directive",
            "item_ref": "test/item",
            "executor_ref": "test/executor",
            "launch_mode": "inline",
            "current_site_id": "site:test",
            "origin_site_id": "site:test",
            "base_project_snapshot_hash": null,
            "result_project_snapshot_hash": null,
            "created_at": "2026-04-22T00:00:00Z",
            "updated_at": "2026-04-22T00:00:00Z",
            "started_at": null,
            "finished_at": null,
            "result": null,
            "error": null,
            "budget": null,
            "artifacts": [],
            "facets": {},
            "last_event_hash": null,
            "last_chain_seq": 0,
            "last_thread_seq": 0,
            "upstream_thread_id": null,
            "requested_by": null,
        });

        write_object(&cas_root, &cs_hash, &cs);
        write_object(&cas_root, &snap_hash, &snap);

        // Write signed head ref
        let head_path = refs_root
            .join("generic/chains/T-root/head");
        fs::create_dir_all(head_path.parent().unwrap()).unwrap();
        let ref_value = serde_json::json!({
            "schema": 1,
            "kind": "signed_ref",
            "ref_path": "chains/T-root/head",
            "target_hash": cs_hash,
            "updated_at": "2026-04-22T00:00:00Z",
            "signer": "test",
            "signature": "test"
        });
        lillux::atomic_write(&head_path, lillux::canonical_json(&ref_value).as_bytes()).unwrap();

        let set = collect_reachable(&cas_root, &refs_root).unwrap();
        assert_eq!(set.chain_root_ids.len(), 1);
        assert_eq!(set.chain_root_ids[0], "T-root");
        assert!(set.object_hashes.contains(&cs_hash));
        assert!(set.object_hashes.contains(&snap_hash));
        assert_eq!(set.object_hashes.len(), 2);
    }

    #[test]
    fn collect_reachable_event_chain() {
        let (_tmp, cas_root, refs_root) = setup_cas_refs();

        let cs_hash = make_hash("cs");
        let snap_hash = make_hash("snap");
        let event_hash = make_hash("evt");
        let prev_event_hash = make_hash("evt0");

        // chain_state points to snapshot and last event
        let cs = serde_json::json!({
            "kind": "chain_state",
            "schema": 1,
            "chain_root_id": "T-root",
            "prev_chain_state_hash": null,
            "last_event_hash": event_hash,
            "last_chain_seq": 2,
            "updated_at": "2026-04-22T00:00:00Z",
            "threads": {
                "T-root": {
                    "snapshot_hash": snap_hash,
                    "last_event_hash": event_hash,
                    "last_thread_seq": 2,
                    "status": "running"
                }
            }
        });

        // snapshot points to last event
        let snap = serde_json::json!({
            "kind": "thread_snapshot",
            "schema": 1,
            "thread_id": "T-root",
            "chain_root_id": "T-root",
            "status": "running",
            "kind_name": "directive",
            "item_ref": "test/item",
            "executor_ref": "test/executor",
            "launch_mode": "inline",
            "current_site_id": "site:test",
            "origin_site_id": "site:test",
            "base_project_snapshot_hash": null,
            "result_project_snapshot_hash": null,
            "created_at": "2026-04-22T00:00:00Z",
            "updated_at": "2026-04-22T00:00:00Z",
            "started_at": "2026-04-22T00:00:00Z",
            "finished_at": null,
            "result": null,
            "error": null,
            "budget": null,
            "artifacts": [],
            "facets": {},
            "last_event_hash": event_hash,
            "last_chain_seq": 2,
            "last_thread_seq": 2,
            "upstream_thread_id": null,
            "requested_by": null,
        });

        // event2 points to event1
        let event = serde_json::json!({
            "kind": "thread_event",
            "schema": 1,
            "chain_root_id": "T-root",
            "chain_seq": 2,
            "thread_id": "T-root",
            "thread_seq": 2,
            "event_type": "test_event",
            "durability": "durable",
            "ts": "2026-04-22T00:00:02Z",
            "prev_chain_event_hash": prev_event_hash,
            "prev_thread_event_hash": prev_event_hash,
            "payload": {}
        });

        // event1 has no prev
        let event1 = serde_json::json!({
            "kind": "thread_event",
            "schema": 1,
            "chain_root_id": "T-root",
            "chain_seq": 1,
            "thread_id": "T-root",
            "thread_seq": 1,
            "event_type": "thread_created",
            "durability": "durable",
            "ts": "2026-04-22T00:00:01Z",
            "prev_chain_event_hash": null,
            "prev_thread_event_hash": null,
            "payload": {}
        });

        write_object(&cas_root, &cs_hash, &cs);
        write_object(&cas_root, &snap_hash, &snap);
        write_object(&cas_root, &event_hash, &event);
        write_object(&cas_root, &prev_event_hash, &event1);

        let head_path = refs_root.join("generic/chains/T-root/head");
        fs::create_dir_all(head_path.parent().unwrap()).unwrap();
        let ref_value = serde_json::json!({
            "schema": 1, "kind": "signed_ref",
            "ref_path": "chains/T-root/head",
            "target_hash": cs_hash,
            "updated_at": "2026-04-22T00:00:00Z",
            "signer": "test", "signature": "test"
        });
        lillux::atomic_write(&head_path, lillux::canonical_json(&ref_value).as_bytes()).unwrap();

        let set = collect_reachable(&cas_root, &refs_root).unwrap();
        assert_eq!(set.object_hashes.len(), 4);
        assert!(set.object_hashes.contains(&cs_hash));
        assert!(set.object_hashes.contains(&snap_hash));
        assert!(set.object_hashes.contains(&event_hash));
        assert!(set.object_hashes.contains(&prev_event_hash));
    }

    #[test]
    fn collect_reachable_project_source() {
        let (_tmp, cas_root, refs_root) = setup_cas_refs();

        let proj_snap_hash = make_hash("ps");
        let manifest_hash = make_hash("sm");
        let item_source_hash = make_hash("is");
        let blob_hash = make_hash("blob");

        // Project snapshot
        let proj_snap = serde_json::json!({
            "kind": "project_snapshot",
            "schema": 2,
            "project_manifest_hash": manifest_hash,
            "parent_hashes": [],
            "created_at": "2026-04-22T00:00:00Z",
            "source": "local"
        });

        // Source manifest
        let manifest = serde_json::json!({
            "kind": "source_manifest",
            "item_source_hashes": {
                "directive:test.md": item_source_hash
            }
        });

        // Item source
        let item_source = serde_json::json!({
            "kind": "item_source",
            "item_ref": "directive:test.md",
            "content_blob_hash": blob_hash,
            "integrity": "sha256"
        });

        // Write blob to blobs dir
        let blob_path = lillux::shard_path(&cas_root, "blobs", &blob_hash, "");
        fs::create_dir_all(blob_path.parent().unwrap()).unwrap();
        fs::write(&blob_path, b"test content").unwrap();

        write_object(&cas_root, &proj_snap_hash, &proj_snap);
        write_object(&cas_root, &manifest_hash, &manifest);
        write_object(&cas_root, &item_source_hash, &item_source);

        let head_path = refs_root.join("projects/test_project/head");
        fs::create_dir_all(head_path.parent().unwrap()).unwrap();
        let ref_value = serde_json::json!({
            "schema": 1, "kind": "signed_ref",
            "ref_path": "projects/test_project",
            "target_hash": proj_snap_hash,
            "updated_at": "2026-04-22T00:00:00Z",
            "signer": "test", "signature": "test"
        });
        lillux::atomic_write(&head_path, lillux::canonical_json(&ref_value).as_bytes()).unwrap();

        let set = collect_reachable(&cas_root, &refs_root).unwrap();
        assert_eq!(set.project_hashes.len(), 1);
        assert!(set.object_hashes.contains(&proj_snap_hash));
        assert!(set.object_hashes.contains(&manifest_hash));
        assert!(set.object_hashes.contains(&item_source_hash));
        assert_eq!(set.object_hashes.len(), 3);
        assert!(set.blob_hashes.contains(&blob_hash));
        assert_eq!(set.blob_hashes.len(), 1);
    }

    #[test]
    fn collect_reachable_ignores_orphan_objects() {
        let (_tmp, cas_root, refs_root) = setup_cas_refs();

        // Write an orphan object not linked from any head
        let orphan_hash = make_hash("orphan");
        write_object(&cas_root, &orphan_hash, &serde_json::json!({
            "kind": "thread_event",
            "schema": 1,
            "chain_root_id": "T-root",
            "chain_seq": 1,
            "thread_id": "T-root",
            "thread_seq": 1,
            "event_type": "orphan",
            "durability": "durable",
            "ts": "2026-04-22T00:00:00Z",
            "prev_chain_event_hash": null,
            "prev_thread_event_hash": null,
            "payload": {}
        }));

        let set = collect_reachable(&cas_root, &refs_root).unwrap();
        assert!(set.object_hashes.is_empty());
        assert!(!set.object_hashes.contains(&orphan_hash));
    }

    #[test]
    fn collect_reachable_chain_history() {
        let (_tmp, cas_root, refs_root) = setup_cas_refs();

        let cs_v1_hash = make_hash("cs1");
        let cs_v2_hash = make_hash("cs2");
        let snap1 = make_hash("s1");
        let snap2 = make_hash("s2");

        // v2 → v1
        let cs_v1 = serde_json::json!({
            "kind": "chain_state",
            "schema": 1,
            "chain_root_id": "T-root",
            "prev_chain_state_hash": null,
            "last_event_hash": null,
            "last_chain_seq": 0,
            "updated_at": "2026-04-22T00:00:00Z",
            "threads": {
                "T-root": {
                    "snapshot_hash": snap1,
                    "last_event_hash": null,
                    "last_thread_seq": 0,
                    "status": "created"
                }
            }
        });

        let cs_v2 = serde_json::json!({
            "kind": "chain_state",
            "schema": 1,
            "chain_root_id": "T-root",
            "prev_chain_state_hash": cs_v1_hash,
            "last_event_hash": null,
            "last_chain_seq": 0,
            "updated_at": "2026-04-22T00:00:01Z",
            "threads": {
                "T-root": {
                    "snapshot_hash": snap2,
                    "last_event_hash": null,
                    "last_thread_seq": 0,
                    "status": "running"
                }
            }
        });

        let snap_v1 = serde_json::json!({
            "kind": "thread_snapshot",
            "schema": 1,
            "thread_id": "T-root",
            "chain_root_id": "T-root",
            "status": "created",
            "kind_name": "directive",
            "item_ref": "test/item",
            "executor_ref": "test/executor",
            "launch_mode": "inline",
            "current_site_id": "site:test",
            "origin_site_id": "site:test",
            "base_project_snapshot_hash": null,
            "result_project_snapshot_hash": null,
            "created_at": "2026-04-22T00:00:00Z",
            "updated_at": "2026-04-22T00:00:00Z",
            "started_at": null,
            "finished_at": null,
            "result": null,
            "error": null,
            "budget": null,
            "artifacts": [],
            "facets": {},
            "last_event_hash": null,
            "last_chain_seq": 0,
            "last_thread_seq": 0,
            "upstream_thread_id": null,
            "requested_by": null,
        });

        let snap_v2 = serde_json::json!({
            "kind": "thread_snapshot",
            "schema": 1,
            "thread_id": "T-root",
            "chain_root_id": "T-root",
            "status": "running",
            "kind_name": "directive",
            "item_ref": "test/item",
            "executor_ref": "test/executor",
            "launch_mode": "inline",
            "current_site_id": "site:test",
            "origin_site_id": "site:test",
            "base_project_snapshot_hash": null,
            "result_project_snapshot_hash": null,
            "created_at": "2026-04-22T00:00:00Z",
            "updated_at": "2026-04-22T00:00:01Z",
            "started_at": "2026-04-22T00:00:01Z",
            "finished_at": null,
            "result": null,
            "error": null,
            "budget": null,
            "artifacts": [],
            "facets": {},
            "last_event_hash": null,
            "last_chain_seq": 0,
            "last_thread_seq": 0,
            "upstream_thread_id": null,
            "requested_by": null,
        });

        write_object(&cas_root, &cs_v1_hash, &cs_v1);
        write_object(&cas_root, &cs_v2_hash, &cs_v2);
        write_object(&cas_root, &snap1, &snap_v1);
        write_object(&cas_root, &snap2, &snap_v2);

        let head_path = refs_root.join("generic/chains/T-root/head");
        fs::create_dir_all(head_path.parent().unwrap()).unwrap();
        let ref_value = serde_json::json!({
            "schema": 1, "kind": "signed_ref",
            "ref_path": "chains/T-root/head",
            "target_hash": cs_v2_hash,
            "updated_at": "2026-04-22T00:00:01Z",
            "signer": "test", "signature": "test"
        });
        lillux::atomic_write(&head_path, lillux::canonical_json(&ref_value).as_bytes()).unwrap();

        let set = collect_reachable(&cas_root, &refs_root).unwrap();
        // Both chain states + both snapshots = 4 objects
        assert_eq!(set.object_hashes.len(), 4);
        assert!(set.object_hashes.contains(&cs_v1_hash));
        assert!(set.object_hashes.contains(&cs_v2_hash));
        assert!(set.object_hashes.contains(&snap1));
        assert!(set.object_hashes.contains(&snap2));
    }

    #[test]
    fn collect_chain_reachable_single_chain() {
        let (_tmp, cas_root, refs_root) = setup_cas_refs();

        let cs_hash = make_hash("cs");
        let snap_hash = make_hash("snap");

        let cs = serde_json::json!({
            "kind": "chain_state",
            "schema": 1,
            "chain_root_id": "T-root",
            "prev_chain_state_hash": null,
            "last_event_hash": null,
            "last_chain_seq": 0,
            "updated_at": "2026-04-22T00:00:00Z",
            "threads": {
                "T-root": {
                    "snapshot_hash": snap_hash,
                    "last_event_hash": null,
                    "last_thread_seq": 0,
                    "status": "created"
                }
            }
        });

        let snap = serde_json::json!({
            "kind": "thread_snapshot",
            "schema": 1,
            "thread_id": "T-root",
            "chain_root_id": "T-root",
            "status": "created",
            "kind_name": "directive",
            "item_ref": "test/item",
            "executor_ref": "test/executor",
            "launch_mode": "inline",
            "current_site_id": "site:test",
            "origin_site_id": "site:test",
            "base_project_snapshot_hash": null,
            "result_project_snapshot_hash": null,
            "created_at": "2026-04-22T00:00:00Z",
            "updated_at": "2026-04-22T00:00:00Z",
            "started_at": null,
            "finished_at": null,
            "result": null,
            "error": null,
            "budget": null,
            "artifacts": [],
            "facets": {},
            "last_event_hash": null,
            "last_chain_seq": 0,
            "last_thread_seq": 0,
            "upstream_thread_id": null,
            "requested_by": null,
        });

        write_object(&cas_root, &cs_hash, &cs);
        write_object(&cas_root, &snap_hash, &snap);

        // Also create a second chain that should NOT be included
        let cs2_hash = make_hash("cs2");
        let cs2 = serde_json::json!({
            "kind": "chain_state",
            "schema": 1,
            "chain_root_id": "T-other",
            "prev_chain_state_hash": null,
            "last_event_hash": null,
            "last_chain_seq": 0,
            "updated_at": "2026-04-22T00:00:00Z",
            "threads": {}
        });
        write_object(&cas_root, &cs2_hash, &cs2);

        let head_path = refs_root.join("generic/chains/T-root/head");
        fs::create_dir_all(head_path.parent().unwrap()).unwrap();
        let ref_value = serde_json::json!({
            "schema": 1, "kind": "signed_ref",
            "ref_path": "chains/T-root/head",
            "target_hash": cs_hash,
            "updated_at": "2026-04-22T00:00:00Z",
            "signer": "test", "signature": "test"
        });
        lillux::atomic_write(&head_path, lillux::canonical_json(&ref_value).as_bytes()).unwrap();

        // Don't write T-other head ref

        let set = collect_chain_reachable(&cas_root, &refs_root, "T-root").unwrap();
        assert_eq!(set.chain_root_ids.len(), 1);
        assert_eq!(set.chain_root_ids[0], "T-root");
        assert!(set.object_hashes.contains(&cs_hash));
        assert!(set.object_hashes.contains(&snap_hash));
        assert!(!set.object_hashes.contains(&cs2_hash));
    }
}
