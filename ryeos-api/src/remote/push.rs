//! CAS push pipeline for remote nodes.
//!
//! Handles the ingest-locally → upload-blobs → push-head pipeline
//! for pushing project content to a remote node.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use base64::Engine as _;

use lillux::cas::{sha256_hex, CasStore};
use ryeos_state::ignore::IgnoreMatcher;
use ryeos_state::objects::SourceManifest;

use crate::remote::client::{BlobUpload, RemoteClient};
use ryeos_app::state::AppState;

/// Result of pushing a project to a remote.
#[derive(Debug)]
pub struct PushResult {
    pub snapshot_hash: String,
    pub manifest_hash: String,
    /// The exact pushed manifest — needed by pull_results() for
    /// conflict detection (can't recompute later; workspace may drift).
    pub manifest: SourceManifest,
    pub manifest_entries: usize,
    pub blobs_uploaded: usize,
    pub blobs_skipped: usize,
}

/// Push a project directory to a remote node.
///
/// 1. Apply the remote's ingest ignore rules (or fall back to local rules)
/// 2. Ingest locally into CAS
/// 3. Build manifest + snapshot
/// 4. Check which blobs the remote already has
/// 5. Upload missing blobs + manifest + snapshot
/// 6. Call push-head to write the HEAD ref
///
/// The `remote_ignore` matcher is the **primary** ignore policy: the
/// manifest is built using the remote's rules so that the pushed content
/// matches what the remote would accept during ingest. If no remote rules
/// are available, falls back to local `ignore`.
pub async fn push_project(
    client: &RemoteClient,
    state: &Arc<AppState>,
    project_path: &Path,
    project_path_for_ref: &str,
    ignore: &IgnoreMatcher,
    remote_ignore: Option<&IgnoreMatcher>,
) -> Result<PushResult> {
    let system_space_dir = &state.config.system_space_dir;

    // 1. Ingest project directory into local CAS using remote's ignore
    //    rules (preferred) or local rules (fallback).
    let local_cas_root = system_space_dir.join(ryeos_engine::AI_DIR).join("state").join("objects");
    let local_cas = CasStore::new(local_cas_root.clone());

    let effective_ignore: &IgnoreMatcher = remote_ignore.unwrap_or(ignore);

    let mut items: HashMap<String, String> = HashMap::new();
    ingest_for_push(&local_cas, &local_cas_root, project_path, project_path, &mut items, effective_ignore)?;

    // 2. Build manifest
    let manifest = SourceManifest { item_source_hashes: items };
    let manifest_hash = local_cas.store_object(&manifest.to_value())?;

    // 3. Build snapshot
    let snapshot = ryeos_state::objects::ProjectSnapshot {
        project_manifest_hash: manifest_hash.clone(),
        user_manifest_hash: None,
        parent_hashes: Vec::new(),
        created_at: lillux::time::iso8601_now(),
        source: "push".to_string(),
    };
    let snapshot_hash = local_cas.store_object(&snapshot.to_value())?;

    // 4. Collect all object hashes we need the remote to have
    let mut all_hashes: Vec<String> = Vec::new();
    for (_rel_path, obj_hash) in &manifest.item_source_hashes {
        all_hashes.push(obj_hash.clone());
        // The item source object also contains a blob reference
        if let Ok(Some(item_obj)) = local_cas.get_object(obj_hash) {
            if let Some(blob_hash) = item_obj.get("content_blob_hash").and_then(|v| v.as_str()) {
                all_hashes.push(blob_hash.to_string());
            }
        }
    }
    all_hashes.push(manifest_hash.clone());
    all_hashes.push(snapshot_hash.clone());
    all_hashes.sort();
    all_hashes.dedup();

    // 5. Check which hashes the remote already has
    let has_resp = client.objects_has(&all_hashes).await?;
    let missing: Vec<String> = has_resp.missing;

    // 6. Upload missing blobs and objects
    let blobs_uploaded = if !missing.is_empty() {
        let mut blobs = Vec::new();
        let mut objects = Vec::new();

        for hash in &missing {
            // Try blob first
            if let Ok(Some(data)) = local_cas.get_blob(hash) {
                let encoded = base64::engine::general_purpose::STANDARD.encode(&data);
                blobs.push(BlobUpload { data: encoded });
            } else if let Ok(Some(value)) = local_cas.get_object(hash) {
                objects.push(value);
            }
        }

        if !blobs.is_empty() || !objects.is_empty() {
            client.objects_put(&blobs, &objects).await?;
        }
        blobs.len() + objects.len()
    } else {
        0
    };

    let blobs_skipped = all_hashes.len() - missing.len();

    // 7. Call push-head
    client.push_head(project_path_for_ref, &snapshot_hash).await?;

    let manifest_entries = manifest.item_source_hashes.len();
    Ok(PushResult {
        snapshot_hash,
        manifest_hash,
        manifest,
        manifest_entries,
        blobs_uploaded,
        blobs_skipped,
    })
}

/// Walk a project directory and ingest files for push.
fn ingest_for_push(
    cas: &CasStore,
    cas_root: &Path,
    root: &Path,
    dir: &Path,
    items: &mut HashMap<String, String>,
    ignore: &IgnoreMatcher,
) -> Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };

    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .to_string();

        // Skip state/ directory
        if rel.starts_with("state/") || rel == "state" {
            continue;
        }

        // Apply ignore rules
        if ignore.is_ignored(&rel) {
            continue;
        }

        if path.is_dir() {
            ingest_for_push(cas, cas_root, root, &path, items, ignore)?;
        } else if path.is_file() {
            let bytes = std::fs::read(&path)?;
            let blob_hash = cas.store_blob(&bytes)?;
            let integrity = sha256_hex(&bytes);

            let item_source = ryeos_state::objects::ItemSource {
                item_ref: rel.clone(),
                content_blob_hash: blob_hash,
                integrity,
                signature_info: None,
                mode: None,
            };
            let obj_hash = cas.store_object(&item_source.to_value())?;
            items.insert(rel, obj_hash);
        }
    }
    Ok(())
}
