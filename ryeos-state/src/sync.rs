//! Object-level sync protocol for CAS-as-truth.
//!
//! Provides four core operations:
//! - `export_chain`: collect all reachable objects (and blobs) for a chain
//! - `reconcile_export`: given remote's hash set, return only missing objects
//! - `import_objects`: write foreign objects to local CAS, verify hashes
//! - `finalize_import`: after import, catch up projection from CAS
//!
//! No transport protocol — sync is object-level. Transport (HTTP, file,
//! etc.) is a separate concern layered on top.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::reachability;
use crate::rebuild;
use crate::StateDb;

/// A single entry in a sync payload (object or blob).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncEntry {
    /// SHA-256 hash of the content.
    pub hash: String,
    /// Whether this is a blob (true) or a JSON object (false).
    pub is_blob: bool,
    /// Raw content bytes.
    pub data: Vec<u8>,
}

/// Payload for exporting / importing objects.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportPayload {
    /// The chain root ID this export covers.
    pub chain_root_id: String,
    /// The chain head hash (chain_state object) at the time of export.
    /// The importer uses this to sign the local chain head ref.
    pub chain_head_hash: String,
    /// All object + blob entries.
    pub entries: Vec<SyncEntry>,
    /// Total serialized size in bytes.
    pub total_bytes: usize,
}

/// Result of importing objects from a remote sync payload.
#[derive(Debug, Clone, Default)]
pub struct ImportResult {
    /// Objects/blobs newly written.
    pub imported: usize,
    /// Objects/blobs already present locally.
    pub already_present: usize,
    /// Hash verification failures (corrupt entries).
    pub hash_mismatches: usize,
    /// Total bytes written.
    pub bytes_written: usize,
}

/// Export all reachable objects for a chain + linked projects.
///
/// Walks the object graph from the signed chain head, collecting every
/// reachable JSON object and blob. Includes project source objects
/// referenced from thread snapshots.
pub fn export_chain(
    cas_root: &Path,
    refs_root: &Path,
    chain_root_id: &str,
) -> Result<ExportPayload> {
    let reachable = reachability::collect_chain_reachable(cas_root, refs_root, chain_root_id)?;

    // Read the chain head hash from the signed ref
    let chain_head_hash = {
        let head_path = refs_root.join("generic/chains").join(chain_root_id).join("head");
        let signed_ref = crate::refs::read_signed_ref(&head_path)
            .with_context(|| format!("failed to read chain head ref for {}", chain_root_id))?;
        signed_ref.target_hash
    };

    let mut entries = Vec::with_capacity(reachable.object_hashes.len() + reachable.blob_hashes.len());
    let mut total_bytes = 0usize;

    // Collect JSON objects
    for hash in &reachable.object_hashes {
        let path = lillux::shard_path(cas_root, "objects", hash, ".json");
        let data = match std::fs::read(&path) {
            Ok(d) => d,
            Err(_) => continue, // referenced but missing — skip
        };
        total_bytes += data.len();
        entries.push(SyncEntry {
            hash: hash.clone(),
            is_blob: false,
            data,
        });
    }

    // Collect blobs
    for hash in &reachable.blob_hashes {
        let path = lillux::shard_path(cas_root, "blobs", hash, "");
        let data = match std::fs::read(&path) {
            Ok(d) => d,
            Err(_) => continue,
        };
        total_bytes += data.len();
        entries.push(SyncEntry {
            hash: hash.clone(),
            is_blob: true,
            data,
        });
    }

    Ok(ExportPayload {
        chain_root_id: chain_root_id.to_string(),
        chain_head_hash,
        entries,
        total_bytes,
    })
}

/// Reconcile export: given remote's set of known hashes, return only
/// the objects/blobs the remote is missing.
///
/// This is the efficient form for incremental sync — the remote sends
/// its hash set, and we reply with only what it needs.
pub fn reconcile_export(
    cas_root: &Path,
    refs_root: &Path,
    chain_root_id: &str,
    remote_has: &HashSet<String>,
) -> Result<ExportPayload> {
    let reachable = reachability::collect_chain_reachable(cas_root, refs_root, chain_root_id)?;

    // Read the chain head hash from the signed ref
    let chain_head_hash = {
        let head_path = refs_root.join("generic/chains").join(chain_root_id).join("head");
        let signed_ref = crate::refs::read_signed_ref(&head_path)
            .with_context(|| format!("failed to read chain head ref for {}", chain_root_id))?;
        signed_ref.target_hash
    };

    // Filter to only objects the remote doesn't have
    let missing_objects: Vec<_> = reachable
        .object_hashes
        .iter()
        .filter(|h| !remote_has.contains(*h))
        .collect();

    let missing_blobs: Vec<_> = reachable
        .blob_hashes
        .iter()
        .filter(|h| !remote_has.contains(*h))
        .collect();

    let mut entries = Vec::with_capacity(missing_objects.len() + missing_blobs.len());
    let mut total_bytes = 0usize;

    for hash in missing_objects {
        let path = lillux::shard_path(cas_root, "objects", hash, ".json");
        let data = match std::fs::read(&path) {
            Ok(d) => d,
            Err(_) => continue,
        };
        total_bytes += data.len();
        entries.push(SyncEntry {
            hash: hash.clone(),
            is_blob: false,
            data,
        });
    }

    for hash in missing_blobs {
        let path = lillux::shard_path(cas_root, "blobs", hash, "");
        let data = match std::fs::read(&path) {
            Ok(d) => d,
            Err(_) => continue,
        };
        total_bytes += data.len();
        entries.push(SyncEntry {
            hash: hash.clone(),
            is_blob: true,
            data,
        });
    }

    Ok(ExportPayload {
        chain_root_id: chain_root_id.to_string(),
        chain_head_hash,
        entries,
        total_bytes,
    })
}

/// Import objects from a remote sync payload into local CAS.
///
/// For each entry:
/// - Verify the hash matches the content (SHA-256 integrity check)
/// - Skip if already present locally
/// - Write to the correct sharded path
///
/// Returns statistics about what was imported.
pub fn import_objects(
    cas_root: &Path,
    payload: &ExportPayload,
) -> Result<ImportResult> {
    let mut result = ImportResult::default();

    for entry in &payload.entries {
        // Verify hash integrity
        let computed = lillux::sha256_hex(&entry.data);
        if computed != entry.hash {
            result.hash_mismatches += 1;
            continue;
        }

        let (namespace, ext) = if entry.is_blob {
            ("blobs", "")
        } else {
            ("objects", ".json")
        };

        let path = lillux::shard_path(cas_root, namespace, &entry.hash, ext);

        // Skip if already present
        if path.exists() {
            result.already_present += 1;
            continue;
        }

        // Write atomically
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create parent dir for {}", entry.hash))?;
        }

        lillux::atomic_write(&path, &entry.data)
            .with_context(|| format!("failed to write CAS object {}", entry.hash))?;

        result.imported += 1;
        result.bytes_written += entry.data.len();
    }

    Ok(result)
}

/// Finalize an import: sign a local chain head ref and catch up projection.
///
/// After importing objects, the imported chain needs a signed head ref
/// to be discoverable via signed heads. Without it, `collect_reachable()`
/// won't find the chain and GC will sweep the imported objects.
///
/// Steps:
/// 1. Verify the chain head object exists in CAS
/// 2. Sign and write a local chain head ref
/// 3. Catch up projection from CAS
pub fn finalize_import(
    state_db: &StateDb,
    chain_root_id: &str,
    chain_head_hash: &str,
    signer: &dyn crate::Signer,
) -> Result<()> {
    let cas_root = state_db.cas_root();
    let refs_root = state_db.refs_root();

    // Step 1: Verify the chain head object exists in CAS
    let head_path = lillux::shard_path(cas_root, "objects", chain_head_hash, ".json");
    if !head_path.exists() {
        anyhow::bail!(
            "chain head {} not found in CAS after import",
            chain_head_hash
        );
    }

    // Step 2: Sign and write local chain head ref
    let ref_path = format!("chains/{}/head", chain_root_id);
    let signed_ref = crate::refs::SignedRef::new(
        ref_path,
        chain_head_hash.to_string(),
        lillux::time::iso8601_now(),
        signer.fingerprint().to_string(),
    );
    let head_ref_path = refs_root.join("generic/chains").join(chain_root_id).join("head");
    crate::refs::write_signed_ref(&head_ref_path, signed_ref, signer)?;

    tracing::info!(
        chain_root_id,
        chain_head_hash,
        "signed local chain head ref after import"
    );

    // Step 3: Catch up projection from CAS
    let projection = state_db.projection();
    let report = rebuild::catch_up_projection(projection, cas_root, refs_root)?;

    if report.chains_updated > 0 || report.threads_restored > 0 {
        tracing::info!(
            chain_root_id,
            chains_updated = report.chains_updated,
            threads_restored = report.threads_restored,
            events_projected = report.events_projected,
            "projection caught up after import"
        );
    }

    Ok(())
}

/// Collect the full set of local hashes (objects + blobs) for a chain.
///
/// Useful for building the `remote_has` set for `reconcile_export`.
pub fn collect_local_hashes(
    cas_root: &Path,
    refs_root: &Path,
    chain_root_id: &str,
) -> Result<HashSet<String>> {
    let reachable = reachability::collect_chain_reachable(cas_root, refs_root, chain_root_id)?;
    let mut hashes = HashSet::with_capacity(
        reachable.object_hashes.len() + reachable.blob_hashes.len(),
    );
    hashes.extend(reachable.object_hashes);
    hashes.extend(reachable.blob_hashes);
    Ok(hashes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signer::TestSigner;
    use crate::Signer as _;
    use std::fs;

    fn make_hash(suffix: &str) -> String {
        format!("{:064}", suffix.as_bytes().iter().fold(0u64, |a, &b| a.wrapping_add(b as u64)))
    }

    fn write_object(cas_root: &Path, hash: &str, value: &serde_json::Value) {
        let path = lillux::shard_path(cas_root, "objects", hash, ".json");
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let canonical = lillux::canonical_json(value);
        lillux::atomic_write(&path, canonical.as_bytes()).unwrap();
    }

    fn write_signed_head(refs_root: &Path, chain_root_id: &str, target_hash: &str) {
        let signer = crate::signer::TestSigner::default();
        let ref_path = format!("chains/{}/head", chain_root_id);
        let signed_ref = crate::refs::SignedRef::new(
            ref_path,
            target_hash.to_string(),
            "2026-04-23T00:00:00Z".to_string(),
            signer.fingerprint().to_string(),
        );
        let head_path = refs_root.join("generic/chains").join(chain_root_id).join("head");
        crate::refs::write_signed_ref(&head_path, signed_ref, &signer).unwrap();
    }

    #[test]
    fn export_chain_collects_all_objects() {
        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("objects");
        let refs_root = tmp.path().join("refs");
        fs::create_dir_all(&cas_root).unwrap();
        fs::create_dir_all(&refs_root).unwrap();

        let cs_hash = make_hash("cs");
        let snap_hash = make_hash("snap");
        let cs = serde_json::json!({
            "kind": "chain_state",
            "schema": 1,
            "chain_root_id": "T-root",
            "prev_chain_state_hash": null,
            "last_event_hash": null,
            "last_chain_seq": 0,
            "updated_at": "2026-04-23T00:00:00Z",
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
            "created_at": "2026-04-23T00:00:00Z",
            "updated_at": "2026-04-23T00:00:00Z",
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
        write_signed_head(&refs_root, "T-root", &cs_hash);

        let payload = export_chain(&cas_root, &refs_root, "T-root").unwrap();
        assert_eq!(payload.chain_root_id, "T-root");
        assert_eq!(payload.entries.len(), 2);

        let hashes: Vec<&str> = payload.entries.iter().map(|e| e.hash.as_str()).collect();
        assert!(hashes.contains(&cs_hash.as_str()));
        assert!(hashes.contains(&snap_hash.as_str()));
        assert!(payload.entries.iter().all(|e| !e.is_blob));
    }

    #[test]
    fn import_objects_writes_and_verifies() {
        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("objects");
        fs::create_dir_all(&cas_root).unwrap();

        let data = b"{\"kind\":\"test\",\"value\":42}";
        let hash = lillux::sha256_hex(data);

        let payload = ExportPayload {
            chain_root_id: "T-test".to_string(),
            chain_head_hash: hash.clone(),
            entries: vec![SyncEntry {
                hash: hash.clone(),
                is_blob: false,
                data: data.to_vec(),
            }],
            total_bytes: data.len(),
        };

        let result = import_objects(&cas_root, &payload).unwrap();
        assert_eq!(result.imported, 1);
        assert_eq!(result.already_present, 0);
        assert_eq!(result.hash_mismatches, 0);

        // Verify file exists
        let path = lillux::shard_path(&cas_root, "objects", &hash, ".json");
        assert!(path.exists());

        // Re-import should be a no-op (already present)
        let result2 = import_objects(&cas_root, &payload).unwrap();
        assert_eq!(result2.imported, 0);
        assert_eq!(result2.already_present, 1);
    }

    #[test]
    fn import_objects_rejects_corrupt_hash() {
        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("objects");
        fs::create_dir_all(&cas_root).unwrap();

        let payload = ExportPayload {
            chain_root_id: "T-test".to_string(),
            chain_head_hash: "00".repeat(32),
            entries: vec![SyncEntry {
                hash: "00".repeat(32),
                is_blob: false,
                data: b"corrupt data".to_vec(),
            }],
            total_bytes: 12,
        };

        let result = import_objects(&cas_root, &payload).unwrap();
        assert_eq!(result.hash_mismatches, 1);
        assert_eq!(result.imported, 0);
    }

    #[test]
    fn import_objects_handles_blobs() {
        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("objects");
        fs::create_dir_all(&cas_root).unwrap();

        let data = b"binary blob content here";
        let hash = lillux::sha256_hex(data);

        let payload = ExportPayload {
            chain_root_id: "T-test".to_string(),
            chain_head_hash: hash.clone(),
            entries: vec![SyncEntry {
                hash: hash.clone(),
                is_blob: true,
                data: data.to_vec(),
            }],
            total_bytes: data.len(),
        };

        let result = import_objects(&cas_root, &payload).unwrap();
        assert_eq!(result.imported, 1);

        let path = lillux::shard_path(&cas_root, "blobs", &hash, "");
        assert!(path.exists());
        assert_eq!(fs::read(&path).unwrap(), data);
    }

    #[test]
    fn reconcile_export_skips_known_hashes() {
        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("objects");
        let refs_root = tmp.path().join("refs");
        fs::create_dir_all(&cas_root).unwrap();
        fs::create_dir_all(&refs_root).unwrap();

        let cs_hash = make_hash("cs");
        let snap_hash = make_hash("snap");
        let cs = serde_json::json!({
            "kind": "chain_state",
            "schema": 1,
            "chain_root_id": "T-root",
            "prev_chain_state_hash": null,
            "last_event_hash": null,
            "last_chain_seq": 0,
            "updated_at": "2026-04-23T00:00:00Z",
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
            "created_at": "2026-04-23T00:00:00Z",
            "updated_at": "2026-04-23T00:00:00Z",
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
        write_signed_head(&refs_root, "T-root", &cs_hash);

        // Remote already has the chain_state
        let mut remote_has = HashSet::new();
        remote_has.insert(cs_hash.clone());

        let payload = reconcile_export(&cas_root, &refs_root, "T-root", &remote_has).unwrap();
        assert_eq!(payload.entries.len(), 1);
        assert_eq!(payload.entries[0].hash, snap_hash);
    }

    /// H6: finalize_import signs a local chain head ref and catches up projection.
    #[test]
    fn finalize_import_signs_local_ref() {
        use crate::StateDb;

        let tmp = tempfile::tempdir().unwrap();
        let state_root = tmp.path();
        let db = StateDb::open(state_root).unwrap();
        let signer = TestSigner::default();

        let cas_root = db.cas_root();
        let refs_root = db.refs_root();

        // Write a chain_state + snapshot into CAS (simulating imported objects)
        let cs_hash = make_hash("cs-finalize");
        let snap_hash = make_hash("snap-finalize");
        let cs = serde_json::json!({
            "kind": "chain_state",
            "schema": 1,
            "chain_root_id": "T-imported",
            "prev_chain_state_hash": null,
            "last_event_hash": null,
            "last_chain_seq": 0,
            "updated_at": "2026-04-23T00:00:00Z",
            "threads": {
                "T-imported": {
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
            "thread_id": "T-imported",
            "chain_root_id": "T-imported",
            "status": "created",
            "kind_name": "directive",
            "item_ref": "test/imported",
            "executor_ref": "test/executor",
            "launch_mode": "inline",
            "current_site_id": "site:test",
            "origin_site_id": "site:test",
            "base_project_snapshot_hash": null,
            "result_project_snapshot_hash": null,
            "created_at": "2026-04-23T00:00:00Z",
            "updated_at": "2026-04-23T00:00:00Z",
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

        write_object(cas_root, &cs_hash, &cs);
        write_object(cas_root, &snap_hash, &snap);

        // Run finalize_import
        finalize_import(&db, "T-imported", &cs_hash, &signer).unwrap();

        // Assert: signed chain head ref exists
        let head_ref_path = refs_root.join("generic/chains/T-imported/head");
        assert!(head_ref_path.exists(), "signed chain head ref should exist");

        // Assert: ref target matches the imported chain head hash
        let signed_ref = crate::refs::read_signed_ref(&head_ref_path).unwrap();
        assert_eq!(signed_ref.target_hash, cs_hash);

        // Assert: projection caught up — thread is queryable
        let thread = crate::queries::get_thread(db.projection(), "T-imported").unwrap();
        assert!(thread.is_some(), "imported thread should appear in projection");
        let thread = thread.unwrap();
        assert_eq!(thread.status, "created");
        assert_eq!(thread.item_ref, "test/imported");
    }
}
