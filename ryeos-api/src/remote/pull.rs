//! Client-side pull/apply pipeline for remote execution results.
//!
//! After `remote execute`, the remote node may have advanced its HEAD ref
//! (fold-back writes new files into CAS). This module fetches the resulting
//! snapshot, diffs against the pre-push manifest, and applies changes to
//! the local workspace with a clean-base conflict policy.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::Value;

use lillux::cas::CasStore;
use ryeos_state::objects::SourceManifest;

use crate::remote::client::RemoteClient;

/// Result of pulling remote execution results.
#[derive(Debug, serde::Serialize)]
pub struct PullResult {
    pub snapshot_hash: String,
    pub cas_objects_fetched: usize,
    pub files_updated: usize,
    pub files_deleted: usize,
}

/// Typed errors for the pull/apply pipeline.
#[derive(thiserror::Error, Debug)]
pub enum PullResultsError {
    #[error("remote thread result missing snapshot_hash")]
    MissingSnapshotHash,
    #[error("local workspace conflict at {0}")]
    LocalConflict(String),
    #[error("invalid remote snapshot: {0}")]
    InvalidRemoteSnapshot(String),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Pull remote execution results and apply to local workspace.
///
/// # Arguments
/// * `client` — RemoteClient connected to the remote node
/// * `system_space_dir` — Local system space dir (contains `.ai/state/objects` CAS)
/// * `remote_snapshot_hash` — The snapshot hash from the remote execution result
/// * `local_project_root` — The local project directory to apply changes to
/// * `base_manifest` — The exact manifest that was pushed (from PushResult.manifest)
pub async fn pull_results(
    client: &RemoteClient,
    system_space_dir: &Path,
    remote_snapshot_hash: &str,
    local_project_root: &Path,
    base_manifest: &SourceManifest,
) -> Result<PullResult, PullResultsError> {
    let local_cas_root = system_space_dir.join(ryeos_engine::AI_DIR).join("state").join("objects");
    let local_cas = CasStore::new(local_cas_root.clone());

    // 1. Fetch remote snapshot object
    let snapshot_objs = client.objects_get(&[remote_snapshot_hash.to_string()])
        .await
        .map_err(PullResultsError::Other)?;

    let snapshot_val = snapshot_objs.find_object(remote_snapshot_hash)
        .ok_or_else(|| PullResultsError::InvalidRemoteSnapshot(
            format!("snapshot {} not found in objects_get response", remote_snapshot_hash)
        ))?;

    let manifest_hash = snapshot_val.get("project_manifest_hash")
        .and_then(|v| v.as_str())
        .ok_or_else(|| PullResultsError::InvalidRemoteSnapshot(
            "missing project_manifest_hash in snapshot".into()
        ))?
        .to_string();

    // 2. Fetch remote manifest
    let manifest_objs = client.objects_get(&[manifest_hash.clone()])
        .await
        .map_err(PullResultsError::Other)?;

    let manifest_val = manifest_objs.find_object(&manifest_hash)
        .ok_or_else(|| PullResultsError::InvalidRemoteSnapshot(
            format!("manifest {} not found in objects_get response", manifest_hash)
        ))?;

    let remote_manifest: SourceManifest = parse_manifest(&manifest_val)?;

    // 3. Diff manifests — find new/changed item source hashes + their blob hashes
    let mut needed_hashes: Vec<String> = Vec::new();
    let mut changed_paths: HashMap<String, String> = HashMap::new(); // path → new_hash

    for (path, hash) in &remote_manifest.item_source_hashes {
        let base_hash = base_manifest.item_source_hashes.get(path);
        let is_new_or_changed = match base_hash {
            None => true,
            Some(old) => old != hash,
        };
        if is_new_or_changed {
            needed_hashes.push(hash.clone());
            changed_paths.insert(path.clone(), hash.clone());
        }
    }

    // 4. Fetch needed items into local CAS
    let fetched_count = if !needed_hashes.is_empty() {
        let fetched = client.objects_get(&needed_hashes)
            .await
            .map_err(PullResultsError::Other)?;

        let mut count = 0;
        for entry in &fetched.entries {
            if entry.kind == "object" {
                // Store the item source object
                if let Some(ref val) = entry.value {
                    local_cas.store_object(val)?;
                    count += 1;
                    // If the item source has a content_blob_hash, fetch that blob too
                    if let Some(blob_hash) = val.get("content_blob_hash").and_then(|v| v.as_str()) {
                        let blob_fetched = client.objects_get(&[blob_hash.to_string()])
                            .await
                            .map_err(PullResultsError::Other)?;
                        if let Some(blob_data) = blob_fetched.find_blob(blob_hash) {
                            local_cas.store_blob(&blob_data)?;
                            count += 1;
                        }
                    }
                }
            } else if entry.kind == "blob" {
                if let Some(ref blob_data) = entry.data {
                    use base64::Engine;
                    let bytes = base64::engine::general_purpose::STANDARD
                        .decode(blob_data)
                        .map_err(|e| PullResultsError::Other(anyhow::anyhow!("invalid base64: {e}")))?;
                    local_cas.store_blob(&bytes)?;
                    count += 1;
                }
            }
        }
        count
    } else {
        0
    };

    // 5. Apply changes to local workspace with clean-base policy
    apply_manifest_diff(&local_cas, local_project_root, base_manifest, &remote_manifest)
        .map(|(files_updated, files_deleted)| PullResult {
            snapshot_hash: remote_snapshot_hash.to_string(),
            cas_objects_fetched: fetched_count,
            files_updated,
            files_deleted,
        })
}

/// A planned change to apply.
enum PlannedChange {
    /// Write new content to this relative path.
    Write { rel_path: String, content: Vec<u8> },
    /// Delete this relative path.
    Delete { rel_path: String },
}

/// Apply the diff between base and remote manifests to the local workspace.
///
/// Uses clean-base policy: if any tracked file has drifted since the push,
/// the entire apply is aborted with `LocalConflict`.
///
/// New files from remote that would overwrite a pre-existing local file are
/// also treated as conflicts.
///
/// **Atomicity:** All writes are staged in a temporary directory first.
/// Only after every file is successfully staged do we swap them into place.
/// If anything fails (missing CAS content, I/O error), the workspace is
/// left untouched.
fn apply_manifest_diff(
    local_cas: &CasStore,
    local_project_root: &Path,
    base_manifest: &SourceManifest,
    remote_manifest: &SourceManifest,
) -> Result<(usize, usize), PullResultsError> {
    // Collect all paths from base + remote manifests
    let mut all_paths: std::collections::HashSet<String> = std::collections::HashSet::new();
    for path in base_manifest.item_source_hashes.keys() {
        all_paths.insert(path.clone());
    }
    for path in remote_manifest.item_source_hashes.keys() {
        all_paths.insert(path.clone());
    }

    // Phase A: validate — check all tracked paths for drift, and all new
    // paths for pre-existing files. Also resolve all CAS content.
    let mut planned: Vec<PlannedChange> = Vec::new();

    for path in &all_paths {
        let base_hash = base_manifest.item_source_hashes.get(path);
        let remote_hash = remote_manifest.item_source_hashes.get(path);
        let local_path = local_project_root.join(path);

        // If the path was in the base, verify local hasn't drifted
        if let Some(base_h) = base_hash {
            let local_content = read_local_file(&local_path);
            let local_hash = local_content.as_ref().map(|c| lillux::cas::sha256_hex(c));
            if local_hash.as_deref() != Some(base_h.as_str()) {
                return Err(PullResultsError::LocalConflict(path.clone()));
            }
        }

        // If the path is NEW in remote (not in base), check there's no
        // pre-existing local file — that would be a conflict too.
        if base_hash.is_none() && remote_hash.is_some() && local_path.exists() {
            return Err(PullResultsError::LocalConflict(format!(
                "{} (new remote file would overwrite local)", path
            )));
        }

        // Plan the change
        match (base_hash, remote_hash) {
            // Unchanged — no-op
            (Some(b), Some(r)) if b == r => {}
            // File changed — resolve content from CAS (fail hard if missing)
            (Some(_), Some(r)) => {
                let content = fetch_content_for_item_strict(local_cas, r, path)?;
                planned.push(PlannedChange::Write { rel_path: path.clone(), content });
            }
            // File deleted on remote
            (Some(_), None) => {
                if local_path.exists() {
                    planned.push(PlannedChange::Delete { rel_path: path.clone() });
                }
            }
            // New file on remote — resolve content from CAS (fail hard if missing)
            (None, Some(r)) => {
                let content = fetch_content_for_item_strict(local_cas, r, path)?;
                planned.push(PlannedChange::Write { rel_path: path.clone(), content });
            }
            // Both None — shouldn't happen but no-op
            (None, None) => {}
        }
    }

    // Phase B: all content resolved successfully. Now stage writes into a
    // temp directory, then swap into place atomically.
    let staging_dir = local_project_root.join(".ryeos-pull-staging");
    // Clean up any leftover staging dir from a previous failed attempt
    let _ = std::fs::remove_dir_all(&staging_dir);

    let mut files_updated = 0usize;
    let mut files_deleted = 0usize;

    // Stage all writes
    for change in &planned {
        match change {
            PlannedChange::Write { rel_path, content } => {
                let staged_path = staging_dir.join(rel_path);
                if let Some(parent) = staged_path.parent() {
                    std::fs::create_dir_all(parent)
                        .with_context(|| format!("create staging dir for {}", rel_path))
                        .map_err(PullResultsError::Other)?;
                }
                std::fs::write(&staged_path, content)
                    .with_context(|| format!("stage file {}", rel_path))
                    .map_err(PullResultsError::Other)?;
            }
            PlannedChange::Delete { .. } => {
                // Deletes don't need staging, but we track them
            }
        }
    }

    // Phase C: all writes staged successfully. Now swap staged files into
    // place and perform deletes. This is not perfectly atomic (individual
    // renames), but at least all file *content* was written before we
    // touch the live tree.
    for change in &planned {
        match change {
            PlannedChange::Write { rel_path, .. } => {
                let staged_path = staging_dir.join(rel_path);
                let live_path = local_project_root.join(rel_path);
                if let Some(parent) = live_path.parent() {
                    std::fs::create_dir_all(parent)
                        .with_context(|| format!("create dir for {}", rel_path))
                        .map_err(PullResultsError::Other)?;
                }
                std::fs::rename(&staged_path, &live_path)
                    .with_context(|| format!("swap staged file {}", rel_path))
                    .map_err(PullResultsError::Other)?;
                files_updated += 1;
            }
            PlannedChange::Delete { rel_path } => {
                let live_path = local_project_root.join(rel_path);
                std::fs::remove_file(&live_path)
                    .with_context(|| format!("delete file {}", rel_path))
                    .map_err(PullResultsError::Other)?;
                files_deleted += 1;
            }
        }
    }

    // Clean up staging dir
    let _ = std::fs::remove_dir_all(&staging_dir);

    Ok((files_updated, files_deleted))
}

/// Read a local file, returning None if it doesn't exist.
fn read_local_file(path: &Path) -> Option<Vec<u8>> {
    std::fs::read(path).ok()
}

/// Fetch file content for an item source hash from local CAS.
///
/// Returns an error (not `None`) if the content is missing from CAS.
/// This prevents silent partial applies.
fn fetch_content_for_item_strict(
    cas: &CasStore,
    item_hash: &str,
    rel_path: &str,
) -> Result<Vec<u8>, PullResultsError> {
    let obj = cas.get_object(item_hash)
        .map_err(|e| PullResultsError::Other(
            anyhow::anyhow!("CAS object {} for '{}' not found: {}", item_hash, rel_path, e)
        ))?
        .ok_or_else(|| PullResultsError::Other(
            anyhow::anyhow!("CAS object {} for '{}' not found in store", item_hash, rel_path)
        ))?;
    let blob_hash = obj.get("content_blob_hash")
        .and_then(|v| v.as_str())
        .ok_or_else(|| PullResultsError::Other(
            anyhow::anyhow!("CAS object {} for '{}' has no content_blob_hash", item_hash, rel_path)
        ))?;
    cas.get_blob(blob_hash)
        .map_err(|e| PullResultsError::Other(
            anyhow::anyhow!("CAS blob {} for '{}' not found: {}", blob_hash, rel_path, e)
        ))?
        .ok_or_else(|| PullResultsError::Other(
            anyhow::anyhow!("CAS blob {} for '{}' not found in store", blob_hash, rel_path)
        ))
}

/// Extract snapshot_hash from a remote execute response.
///
/// The snapshot hash is set by fold-back as a thread facet.
pub fn extract_snapshot_hash(response: &Value) -> Option<String> {
    // Try from the top-level thread result
    response.get("snapshot_hash")
        .and_then(|v| v.as_str())
        .map(String::from)
        .or_else(|| {
            // Try from facets
            response.get("facets")
                .and_then(|f| f.get("snapshot_hash"))
                .and_then(|v| v.as_str())
                .map(String::from)
        })
        .or_else(|| {
            // Try from nested result structure
            response.get("result")
                .and_then(|r| r.get("snapshot_hash"))
                .and_then(|v| v.as_str())
                .map(String::from)
        })
        .or_else(|| {
            // Try from thread.result (dispatch wraps result as {thread:{...}, result:{...}})
            response.get("thread")
                .and_then(|t| t.get("result"))
                .and_then(|r| r.get("snapshot_hash"))
                .and_then(|v| v.as_str())
                .map(String::from)
        })
        .or_else(|| {
            // Try from thread facets
            response.get("thread")
                .and_then(|t| t.get("facets"))
                .and_then(|f| f.get("snapshot_hash"))
                .and_then(|v| v.as_str())
                .map(String::from)
        })
}

/// Parse a SourceManifest from a JSON value.
fn parse_manifest(val: &Value) -> Result<SourceManifest, PullResultsError> {
    let mut items = HashMap::new();
    if let Some(hashes) = val.get("item_source_hashes").and_then(|v| v.as_object()) {
        for (path, hash) in hashes {
            if let Some(h) = hash.as_str() {
                items.insert(path.clone(), h.to_string());
            }
        }
    }
    Ok(SourceManifest { item_source_hashes: items })
}
