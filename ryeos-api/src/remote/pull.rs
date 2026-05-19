//! Client-side pull/apply pipeline for remote execution results.
//!
//! After `remote execute`, the remote node may have advanced its HEAD ref
//! (fold-back writes new files into CAS). This module fetches the resulting
//! snapshot, diffs against the pre-push manifest, and applies changes to
//! the local workspace with a clean-base conflict policy.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

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
    /// User-space files updated by the symmetric user pull-back.
    /// Always 0 when the pushed snapshot had no user manifest.
    pub user_files_updated: usize,
    /// User-space files deleted by the symmetric user pull-back.
    pub user_files_deleted: usize,
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
    /// Result snapshot returned by the remote does not descend from the
    /// snapshot we pushed. Defends against a misconfigured or
    /// hostile remote handing us an unrelated snapshot to apply.
    #[error("result snapshot {result} is not a descendant of pushed snapshot {pushed}")]
    UnrelatedSnapshot { pushed: String, result: String },
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Pull remote execution results and apply to local workspace.
///
/// # Arguments
/// * `client` — RemoteClient connected to the remote node
/// * `system_space_dir` — Local system space dir (contains `.ai/state/objects` CAS)
/// * `pushed_snapshot_hash` — The snapshot we pushed (lineage anchor). The
///   result snapshot must equal this OR list it in `parent_hashes` (direct
///   parent only, per resolved design decision).
/// * `remote_snapshot_hash` — The snapshot hash from the remote execution result
/// * `local_project_root` — The local project directory to apply changes to
/// * `base_manifest` — The exact manifest that was pushed (from PushResult.manifest)
/// * `base_user_manifest` — The exact user manifest that was pushed
///   (from `PushResult.user_manifest`). When `None`, the user pull-back
///   step is skipped — the caller didn't push a user manifest, so
///   there's no symmetric base to diff against.
pub async fn pull_results(
    client: &RemoteClient,
    system_space_dir: &Path,
    pushed_snapshot_hash: &str,
    remote_snapshot_hash: &str,
    local_project_root: &Path,
    base_manifest: &SourceManifest,
    base_user_manifest: Option<&SourceManifest>,
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

    // 1a. Lineage check.
    verify_snapshot_lineage(&snapshot_val, pushed_snapshot_hash, remote_snapshot_hash)?;

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

    // 5. Apply project changes to local workspace with clean-base policy
    let (files_updated, files_deleted) =
        apply_manifest_diff(&local_cas, local_project_root, base_manifest, &remote_manifest)?;

    // 6. Symmetric user-space pull-back. Only runs when we pushed a
    //    user manifest AND the remote result snapshot carries one.
    //    Defense-in-depth: every remote-supplied user path is
    //    re-validated against the sync allow-list before being
    //    applied, in case a buggy/hostile remote returns paths
    //    outside the agreed set.
    let mut user_fetched = 0usize;
    let (user_files_updated, user_files_deleted) = match (
        base_user_manifest,
        snapshot_val.get("user_manifest_hash").and_then(|v| v.as_str()),
    ) {
        (Some(base_um), Some(remote_user_mh)) => {
            // Fetch remote user manifest.
            let user_manifest_objs = client
                .objects_get(&[remote_user_mh.to_string()])
                .await
                .map_err(PullResultsError::Other)?;
            let user_manifest_val = user_manifest_objs
                .find_object(remote_user_mh)
                .ok_or_else(|| {
                    PullResultsError::InvalidRemoteSnapshot(format!(
                        "user manifest {} not found in objects_get response",
                        remote_user_mh
                    ))
                })?;
            let remote_user_manifest: SourceManifest = parse_manifest(&user_manifest_val)?;

            // Reject any path that escapes the allow-list. Same
            // component-wise check the push side validates against.
            ryeos_state::user_sync::validate_user_manifest_paths(&remote_user_manifest)
                .map_err(|e| {
                    PullResultsError::InvalidRemoteSnapshot(format!(
                        "remote user manifest violates sync allow-list: {e}"
                    ))
                })?;

            // Fetch any items/blobs the local CAS doesn't already have.
            let mut user_needed: Vec<String> = Vec::new();
            for (path, hash) in &remote_user_manifest.item_source_hashes {
                if base_um.item_source_hashes.get(path) != Some(hash) {
                    user_needed.push(hash.clone());
                }
            }
            if !user_needed.is_empty() {
                let fetched = client
                    .objects_get(&user_needed)
                    .await
                    .map_err(PullResultsError::Other)?;
                for entry in &fetched.entries {
                    if entry.kind == "object" {
                        if let Some(ref val) = entry.value {
                            local_cas.store_object(val)?;
                            user_fetched += 1;
                            if let Some(blob_hash) =
                                val.get("content_blob_hash").and_then(|v| v.as_str())
                            {
                                let blob_fetched = client
                                    .objects_get(&[blob_hash.to_string()])
                                    .await
                                    .map_err(PullResultsError::Other)?;
                                if let Some(blob_data) = blob_fetched.find_blob(blob_hash) {
                                    local_cas.store_blob(&blob_data)?;
                                    user_fetched += 1;
                                }
                            }
                        }
                    } else if entry.kind == "blob" {
                        if let Some(ref blob_data) = entry.data {
                            use base64::Engine;
                            let bytes = base64::engine::general_purpose::STANDARD
                                .decode(blob_data)
                                .map_err(|e| {
                                    PullResultsError::Other(anyhow::anyhow!(
                                        "invalid base64: {e}"
                                    ))
                                })?;
                            local_cas.store_blob(&bytes)?;
                            user_fetched += 1;
                        }
                    }
                }
            }

            // Apply to the local user-space `.ai/` root. user_root()
            // returns ~/.ryeos/; we apply under <user_root>/.ai/ so
            // the relative paths in the manifest land correctly.
            let local_user_root = ryeos_engine::roots::user_root().map_err(|e| {
                PullResultsError::Other(anyhow::anyhow!(
                    "user-space pull-back: cannot resolve user_root: {e}"
                ))
            })?;
            let local_user_ai = local_user_root.join(ryeos_engine::AI_DIR);
            apply_manifest_diff(&local_cas, &local_user_ai, base_um, &remote_user_manifest)?
        }
        _ => (0, 0),
    };

    Ok(PullResult {
        snapshot_hash: remote_snapshot_hash.to_string(),
        cas_objects_fetched: fetched_count + user_fetched,
        files_updated,
        files_deleted,
        user_files_updated,
        user_files_deleted,
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
/// **Atomicity:** All writes are staged in a temporary directory first
/// (Phase B). Only after staging succeeds do we backup originals and swap
/// them into place (Phase C). If any swap operation fails, all previously
/// applied changes are rolled back from the backup, restoring the workspace
/// to its pre-apply state. The workspace is never left in a partial state.
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
        let local_path = safe_join(local_project_root, path)?;

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

    // Phase B: all content resolved successfully. Stage writes into a
    // temp directory. This is the only phase that touches the filesystem
    // before Phase C — if anything fails here the workspace is untouched.
    let staging_dir = local_project_root.join(".ryeos-pull-staging");
    // Clean up any leftover staging dir from a previous failed attempt
    let _ = std::fs::remove_dir_all(&staging_dir);

    for change in &planned {
        if let PlannedChange::Write { rel_path, content } = change {
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
    }

    // Phase C: atomic swap. We backup originals to `.ryeos-pull-backup/`,
    // then swap staged files into place and perform deletes. If anything
    // fails mid-swap, we roll back from the backup so the workspace is
    // never left in a partial state.
    let backup_dir = local_project_root.join(".ryeos-pull-backup");
    let _ = std::fs::remove_dir_all(&backup_dir);

    let mut files_updated = 0usize;
    let mut files_deleted = 0usize;

    // C.1: Backup any live files that will be overwritten or deleted.
    for change in &planned {
        let rel_path = match change {
            PlannedChange::Write { rel_path, .. } => rel_path,
            PlannedChange::Delete { rel_path } => rel_path,
        };
        let live_path = local_project_root.join(rel_path);
        if live_path.exists() {
            let backup_path = backup_dir.join(rel_path);
            if let Some(parent) = backup_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            // Copy (not rename) so the live file stays in place until swap.
            std::fs::copy(&live_path, &backup_path)
                .with_context(|| format!("backup {} for rollback", rel_path))
                .map_err(PullResultsError::Other)?;
        }
    }

    // C.2: Apply writes and deletes. On any failure, roll back.
    let apply_result = apply_with_rollback(
        &planned,
        &staging_dir,
        &backup_dir,
        local_project_root,
        &mut files_updated,
        &mut files_deleted,
    );

    // C.3: Clean up staging + backup dirs regardless of outcome.
    let _ = std::fs::remove_dir_all(&staging_dir);
    let _ = std::fs::remove_dir_all(&backup_dir);

    apply_result
}

/// Apply planned writes and deletes with rollback on failure.
///
/// If any individual operation fails, restores all previously-applied
/// changes from the backup directory so the workspace is left in its
/// original state.
fn apply_with_rollback(
    planned: &[PlannedChange],
    staging_dir: &Path,
    backup_dir: &Path,
    local_project_root: &Path,
    files_updated: &mut usize,
    files_deleted: &mut usize,
) -> Result<(usize, usize), PullResultsError> {
    // Track what we've applied so far for rollback.
    let mut applied_writes: Vec<String> = Vec::new();
    let mut applied_deletes: Vec<String> = Vec::new();

    for change in planned {
        match change {
            PlannedChange::Write { rel_path, .. } => {
                let staged_path = staging_dir.join(rel_path);
                let live_path = safe_join(local_project_root, rel_path)?;
                if let Some(parent) = live_path.parent() {
                    if let Err(e) = std::fs::create_dir_all(parent) {
                        rollback_writes_and_deletes(
                            &applied_writes, &applied_deletes,
                            backup_dir, local_project_root,
                        );
                        return Err(PullResultsError::Other(
                            anyhow::anyhow!("create dir for {}: {e}", rel_path)
                        ));
                    }
                }
                if let Err(e) = std::fs::rename(&staged_path, &live_path) {
                    // rename may fail cross-device; fall back to copy+delete
                    if let Err(e2) = std::fs::copy(&staged_path, &live_path)
                        .and_then(|_| std::fs::remove_file(&staged_path))
                    {
                        rollback_writes_and_deletes(
                            &applied_writes, &applied_deletes,
                            backup_dir, local_project_root,
                        );
                        return Err(PullResultsError::Other(
                            anyhow::anyhow!("swap staged file {}: {e}, fallback copy: {e2}", rel_path)
                        ));
                    }
                }
                applied_writes.push(rel_path.clone());
                *files_updated += 1;
            }
            PlannedChange::Delete { rel_path } => {
                let live_path = safe_join(local_project_root, rel_path)?;
                if let Err(e) = std::fs::remove_file(&live_path) {
                    rollback_writes_and_deletes(
                        &applied_writes, &applied_deletes,
                        backup_dir, local_project_root,
                    );
                    return Err(PullResultsError::Other(
                        anyhow::anyhow!("delete file {}: {e}", rel_path)
                    ));
                }
                applied_deletes.push(rel_path.clone());
                *files_deleted += 1;
            }
        }
    }

    Ok((*files_updated, *files_deleted))
}

/// Roll back applied writes and deletes by restoring from backup.
///
/// For files that had a backup (existed before apply), the backup is
/// restored over the live path. For files that had **no** backup (new
/// files created during apply), the live path is **removed** entirely.
fn rollback_writes_and_deletes(
    applied_writes: &[String],
    applied_deletes: &[String],
    backup_dir: &Path,
    local_project_root: &Path,
) {
    // Undo writes: restore backup if it exists, otherwise remove the
    // newly-created file.
    for rel_path in applied_writes {
        let backup_path = backup_dir.join(rel_path);
        let live_path = local_project_root.join(rel_path);
        if backup_path.exists() {
            let _ = std::fs::copy(&backup_path, &live_path);
        } else {
            // This was a new file — remove it to restore pre-apply state.
            let _ = std::fs::remove_file(&live_path);
        }
    }
    // Restore deleted files from backup.
    for rel_path in applied_deletes {
        let backup_path = backup_dir.join(rel_path);
        let live_path = local_project_root.join(rel_path);
        if backup_path.exists() {
            if let Some(parent) = live_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::copy(&backup_path, &live_path);
        }
    }
}

/// Read a local file, returning None if it doesn't exist.
fn read_local_file(path: &Path) -> Option<Vec<u8>> {
    std::fs::read(path).ok()
}

/// Verify that a result snapshot is a valid descendant of the pushed
/// snapshot (lineage check).
///
/// Accepts:
/// - `result == pushed` (no-op execution case)
/// - `pushed ∈ result.parent_hashes` (direct parent only — no recursive walk)
///
/// Rejects everything else with `UnrelatedSnapshot`.
///
/// Direct-parent-only is the resolved design decision: tighter contract,
/// no risk of accepting deep-history junk, no recursive object fetches.
fn verify_snapshot_lineage(
    result_snapshot: &Value,
    pushed_snapshot_hash: &str,
    result_snapshot_hash: &str,
) -> Result<(), PullResultsError> {
    if result_snapshot_hash == pushed_snapshot_hash {
        return Ok(());
    }
    let parents_match = result_snapshot
        .get("parent_hashes")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .any(|p| p.as_str() == Some(pushed_snapshot_hash))
        })
        .unwrap_or(false);
    if parents_match {
        Ok(())
    } else {
        Err(PullResultsError::UnrelatedSnapshot {
            pushed: pushed_snapshot_hash.to_string(),
            result: result_snapshot_hash.to_string(),
        })
    }
}

/// Safely join a base directory with a relative path from a remote manifest.
///
/// Rejects:
/// - Absolute paths
/// - Paths containing `..` components
/// - Paths containing NUL bytes
/// - Paths that, after lexical normalization, escape the base directory
/// - Any **intermediate path component that resolves to a symlink** in the
///   live workspace (/ item 5). Without this, a hostile remote could
///   trick us into writing through a symlink already present locally
///   (e.g. `src/` pre-symlinked to `/etc/`).
///
/// Uses Component matching for lexical safety AND per-component
/// `symlink_metadata` to catch symlink-via-workspace escapes. Does NOT
/// call `canonicalize` (which would *follow* symlinks rather than
/// detect them).
fn safe_join(base: &Path, rel: &str) -> Result<PathBuf, PullResultsError> {
    if rel.is_empty() {
        return Ok(base.to_path_buf());
    }
    // Reject absolute paths
    if rel.starts_with('/') {
        return Err(PullResultsError::Other(anyhow::anyhow!(
            "rejecting absolute path '{}' from remote manifest",
            rel
        )));
    }
    // Reject NUL bytes
    if rel.contains('\0') {
        return Err(PullResultsError::Other(anyhow::anyhow!(
            "rejecting path with NUL byte from remote manifest: '{}'",
            rel.replace('\0', "\\0")
        )));
    }
    // Lexical normalization and escape check via Component matching
    let mut normalized = PathBuf::new();
    for component in std::path::Path::new(rel).components() {
        match component {
            std::path::Component::CurDir => {} // skip
            std::path::Component::ParentDir => {
                // Walking above base — reject
                if !normalized.pop() {
                    return Err(PullResultsError::Other(anyhow::anyhow!(
                        "rejecting path '{}' from remote manifest: escapes base directory (.. traversal)",
                        rel
                    )));
                }
            }
            std::path::Component::Normal(seg) => {
                normalized.push(seg);
            }
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                return Err(PullResultsError::Other(anyhow::anyhow!(
                    "rejecting path '{}' from remote manifest: unexpected component type",
                    rel
                )));
            }
        }
    }

    // Symlink-escape guard: walk each intermediate component (NOT the
    // final filename) under base and refuse if any is a symlink.
    // The final file may legitimately not exist yet (new file) or be a
    // regular file we plan to overwrite; intermediate dirs must not be
    // symlinks because writes would land outside `base`.
    let mut walker = base.to_path_buf();
    let components: Vec<_> = normalized.components().collect();
    let last_idx = components.len().saturating_sub(1);
    for (i, comp) in components.iter().enumerate() {
        if let std::path::Component::Normal(seg) = comp {
            walker.push(seg);
            // Skip the final component — checking it as a symlink is a
            // separate concern (overwriting an existing file is fine).
            if i == last_idx {
                break;
            }
            // Use symlink_metadata so we see the link itself, not its target.
            match std::fs::symlink_metadata(&walker) {
                Ok(meta) if meta.file_type().is_symlink() => {
                    return Err(PullResultsError::Other(anyhow::anyhow!(
                        "rejecting path '{}' from remote manifest: \
                         intermediate component '{}' is a symlink in the \
                         local workspace; refusing to write through it",
                        rel,
                        walker.display()
                    )));
                }
                // Doesn't exist yet → fine, we'll mkdir it
                Err(_) => {}
                Ok(_) => {}
            }
        }
    }

    Ok(base.join(&normalized))
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use lillux::cas::CasStore;

    /// Helper: create a CAS with a single blob and its corresponding item
    /// source object. Returns the item hash.
    fn store_blob_and_item(cas: &CasStore, content: &[u8]) -> String {
        let blob_hash = cas.store_blob(content).unwrap();
        let item = serde_json::json!({
            "content_blob_hash": blob_hash,
        });
        cas.store_object(&item).unwrap()
    }

    fn make_manifest(items: &[(&str, &str)]) -> SourceManifest {
        SourceManifest {
            item_source_hashes: items.iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    // ── R3-3a: pull/apply conflict detection ──

    #[test]
    fn apply_detects_local_conflict_on_tracked_file() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path();

        // Write a file with hash "aaa..." but manifest says it should be "bbb..."
        std::fs::write(project_root.join("file.txt"), "original content").unwrap();

        let base = make_manifest(&[("file.txt", "sha256:original_hash")]);
        let remote = make_manifest(&[("file.txt", "sha256:changed_hash")]);

        let cas_root = tmp.path().join("cas");
        let cas = CasStore::new(cas_root);

        let result = apply_manifest_diff(&cas, project_root, &base, &remote);
        match result {
            Err(PullResultsError::LocalConflict(path)) => {
                assert_eq!(path, "file.txt");
            }
            other => panic!("expected LocalConflict, got {:?}", other),
        }

        // Workspace should be untouched
        assert_eq!(
            std::fs::read_to_string(project_root.join("file.txt")).unwrap(),
            "original content"
        );
    }

    #[test]
    fn apply_detects_new_remote_file_overwrites_local() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path();

        // A file exists locally that the remote also wants to create
        std::fs::write(project_root.join("new.txt"), "local content").unwrap();

        let base = make_manifest(&[]); // empty base — file wasn't tracked
        let remote = make_manifest(&[("new.txt", "sha256:some_hash")]);

        let cas_root = tmp.path().join("cas");
        let cas = CasStore::new(cas_root);

        let result = apply_manifest_diff(&cas, project_root, &base, &remote);
        match result {
            Err(PullResultsError::LocalConflict(msg)) => {
                assert!(msg.contains("new.txt"), "got: {msg}");
                assert!(msg.contains("new remote file would overwrite local"), "got: {msg}");
            }
            other => panic!("expected LocalConflict, got {:?}", other),
        }
    }

    // ── R3-3a: rollback on missing CAS content ──

    #[test]
    fn apply_rollback_on_missing_cas_leaves_workspace_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path();

        // Two files in base and remote. File A is changed (content in CAS).
        // File B is changed but CAS content is MISSING. This should cause
        // the entire apply to fail during Phase A (validation), before
        // any files are written.
        let content_a = "original A";
        let content_b = "original B";
        std::fs::write(project_root.join("a.txt"), content_a).unwrap();
        std::fs::write(project_root.join("b.txt"), content_b).unwrap();

        let cas_root = tmp.path().join("cas");
        let cas = CasStore::new(cas_root);

        // Store content for file A only
        let hash_a = store_blob_and_item(&cas, b"new A content");
        let hash_a_old = lillux::cas::sha256_hex(content_a.as_bytes());
        let hash_b_old = lillux::cas::sha256_hex(content_b.as_bytes());
        // File B's hash doesn't exist in CAS — will trigger strict failure

        let base = make_manifest(&[
            ("a.txt", &hash_a_old),
            ("b.txt", &hash_b_old),
        ]);
        let remote = make_manifest(&[
            ("a.txt", &hash_a),
            ("b.txt", "sha256:missing_hash"),
        ]);

        let result = apply_manifest_diff(&cas, project_root, &base, &remote);
        assert!(result.is_err(), "should fail on missing CAS content");

        // Workspace should be completely untouched — no partial apply
        assert_eq!(
            std::fs::read_to_string(project_root.join("a.txt")).unwrap(),
            "original A",
            "file a.txt should NOT have been updated"
        );
        assert_eq!(
            std::fs::read_to_string(project_root.join("b.txt")).unwrap(),
            "original B",
            "file b.txt should NOT have been updated"
        );
    }

    #[test]
    fn apply_happy_path_writes_and_deletes() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path();

        // File A: changed, File B: deleted, File C: unchanged, File D: new
        let content_a = "old A";
        let content_b = "to be deleted";
        let content_c = "unchanged";
        std::fs::write(project_root.join("a.txt"), content_a).unwrap();
        std::fs::write(project_root.join("b.txt"), content_b).unwrap();
        std::fs::write(project_root.join("c.txt"), content_c).unwrap();

        let cas_root = tmp.path().join("cas");
        let cas = CasStore::new(cas_root);

        let hash_a_old = lillux::cas::sha256_hex(content_a.as_bytes());
        let hash_b_old = lillux::cas::sha256_hex(content_b.as_bytes());
        let hash_c = lillux::cas::sha256_hex(content_c.as_bytes());

        let hash_a_new = store_blob_and_item(&cas, b"new A");
        let hash_d = store_blob_and_item(&cas, b"brand new D");

        let base = make_manifest(&[
            ("a.txt", &hash_a_old),
            ("b.txt", &hash_b_old),
            ("c.txt", &hash_c),
        ]);
        let remote = make_manifest(&[
            ("a.txt", &hash_a_new),
            // b.txt absent → deleted
            ("c.txt", &hash_c),
            ("d.txt", &hash_d),
        ]);

        let (updated, deleted) = apply_manifest_diff(&cas, project_root, &base, &remote).unwrap();
        assert_eq!(updated, 2); // a.txt + d.txt
        assert_eq!(deleted, 1); // b.txt

        assert_eq!(std::fs::read_to_string(project_root.join("a.txt")).unwrap(), "new A");
        assert!(!project_root.join("b.txt").exists(), "b.txt should be deleted");
        assert_eq!(std::fs::read_to_string(project_root.join("c.txt")).unwrap(), "unchanged");
        assert_eq!(std::fs::read_to_string(project_root.join("d.txt")).unwrap(), "brand new D");
    }

    // ── extract_snapshot_hash coverage ──

    #[test]
    fn extract_snapshot_hash_top_level() {
        let v = serde_json::json!({ "snapshot_hash": "abc123" });
        assert_eq!(extract_snapshot_hash(&v), Some("abc123".into()));
    }

    #[test]
    fn extract_snapshot_hash_from_facets() {
        let v = serde_json::json!({ "facets": { "snapshot_hash": "fac123" } });
        assert_eq!(extract_snapshot_hash(&v), Some("fac123".into()));
    }

    #[test]
    fn extract_snapshot_hash_from_thread_result() {
        let v = serde_json::json!({ "thread": { "result": { "snapshot_hash": "thr123" } } });
        assert_eq!(extract_snapshot_hash(&v), Some("thr123".into()));
    }

    #[test]
    fn extract_snapshot_hash_missing() {
        let v = serde_json::json!({ "other": "data" });
        assert!(extract_snapshot_hash(&v).is_none());
    }

    // ── R4-1: Mid-swap rollback removes newly-created files ──

    #[test]
    fn rollback_removes_new_files_not_in_backup() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path();
        let backup_dir = tmp.path().join("backup");
        std::fs::create_dir_all(&backup_dir).unwrap();

        // Simulate: a.txt existed before (has backup), new.txt was
        // newly created (no backup). Rollback should restore a.txt
        // and DELETE new.txt.
        std::fs::write(project_root.join("a.txt"), "new content").unwrap();
        std::fs::write(backup_dir.join("a.txt"), "original content").unwrap();
        std::fs::write(project_root.join("new.txt"), "brand new").unwrap();
        // No backup for new.txt — it was created during apply.

        rollback_writes_and_deletes(
            &["a.txt".into(), "new.txt".into()],
            &[],
            &backup_dir,
            project_root,
        );

        // a.txt restored from backup
        assert_eq!(
            std::fs::read_to_string(project_root.join("a.txt")).unwrap(),
            "original content"
        );
        // new.txt removed — it didn't exist before apply
        assert!(
            !project_root.join("new.txt").exists(),
            "new.txt should have been removed by rollback"
        );
    }

    #[test]
    fn rollback_restores_deleted_files_from_backup() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path();
        let backup_dir = tmp.path().join("backup");
        std::fs::create_dir_all(&backup_dir).unwrap();

        // Simulate: b.txt was deleted during apply (file removed, backup exists)
        std::fs::write(backup_dir.join("b.txt"), "resurrected").unwrap();
        // b.txt does NOT exist in project_root (was deleted)

        rollback_writes_and_deletes(
            &[],
            &["b.txt".into()],
            &backup_dir,
            project_root,
        );

        assert_eq!(
            std::fs::read_to_string(project_root.join("b.txt")).unwrap(),
            "resurrected"
        );
    }

    // ── safe_join path normalization tests ──

    #[test]
    fn safe_join_accepts_normal_relative_path() {
        let base = Path::new("/project");
        let result = safe_join(base, "src/lib.rs").unwrap();
        assert_eq!(result, PathBuf::from("/project/src/lib.rs"));
    }

    #[test]
    fn safe_join_accepts_nested_path() {
        let base = Path::new("/project");
        let result = safe_join(base, "a/b/c.txt").unwrap();
        assert_eq!(result, PathBuf::from("/project/a/b/c.txt"));
    }

    #[test]
    fn safe_join_strips_dot_components() {
        let base = Path::new("/project");
        let result = safe_join(base, "./src/./lib.rs").unwrap();
        assert_eq!(result, PathBuf::from("/project/src/lib.rs"));
    }

    #[test]
    fn safe_join_rejects_absolute_path() {
        let base = Path::new("/project");
        let result = safe_join(base, "/etc/passwd");
        assert!(result.is_err());
        let msg = format!("{:?}", result.unwrap_err());
        assert!(msg.contains("absolute"));
    }

    #[test]
    fn safe_join_rejects_dotdot_traversal() {
        let base = Path::new("/project");
        let result = safe_join(base, "../../../etc/passwd");
        assert!(result.is_err());
        let msg = format!("{:?}", result.unwrap_err());
        assert!(msg.contains("..") || msg.contains("escapes"));
    }

    #[test]
    fn safe_join_rejects_nul_byte() {
        let base = Path::new("/project");
        let result = safe_join(base, "foo\0bar");
        assert!(result.is_err());
    }

    #[test]
    fn safe_join_accepts_path_with_internal_dotdot_that_stays_within() {
        let base = Path::new("/project");
        // a/../b stays within base → allowed
        let result = safe_join(base, "a/../b.txt").unwrap();
        assert_eq!(result, PathBuf::from("/project/b.txt"));
    }

    // ── snapshot lineage check tests ──

    #[test]
    fn lineage_accepts_identical_snapshot() {
        // No-op execution: result hash == pushed hash
        let snap = serde_json::json!({
            "project_manifest_hash": "manifest_x",
            "parent_hashes": [],
        });
        verify_snapshot_lineage(&snap, "abc", "abc").expect("identical hash must accept");
    }

    #[test]
    fn lineage_accepts_direct_parent() {
        // result.parent_hashes contains pushed hash
        let snap = serde_json::json!({
            "project_manifest_hash": "manifest_y",
            "parent_hashes": ["pushed_xxx", "other_yyy"],
        });
        verify_snapshot_lineage(&snap, "pushed_xxx", "result_zzz")
            .expect("direct parent must accept");
    }

    #[test]
    fn lineage_rejects_unrelated_snapshot() {
        // Different hash, pushed NOT in parents
        let snap = serde_json::json!({
            "project_manifest_hash": "manifest_z",
            "parent_hashes": ["unrelated_aaa"],
        });
        let err = verify_snapshot_lineage(&snap, "pushed_xxx", "result_zzz")
            .expect_err("unrelated snapshot must reject");
        match err {
            PullResultsError::UnrelatedSnapshot { pushed, result } => {
                assert_eq!(pushed, "pushed_xxx");
                assert_eq!(result, "result_zzz");
            }
            other => panic!("expected UnrelatedSnapshot, got {:?}", other),
        }
    }

    #[test]
    fn lineage_rejects_missing_parent_hashes() {
        // Different hash, no parent_hashes field at all
        let snap = serde_json::json!({
            "project_manifest_hash": "manifest_z",
        });
        let err = verify_snapshot_lineage(&snap, "pushed_xxx", "result_zzz")
            .expect_err("missing parents on different hash must reject");
        assert!(matches!(err, PullResultsError::UnrelatedSnapshot { .. }));
    }

    #[test]
    #[cfg(unix)]
    fn safe_join_rejects_symlinked_intermediate_dir() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path();
        let outside = tempfile::tempdir().unwrap();

        // Create a pre-existing symlink inside the workspace pointing outside.
        // A naive safe_join would lexically validate "src/file.txt" and then
        // write through the symlink into `outside`.
        symlink(outside.path(), project.join("src")).unwrap();

        let err = safe_join(project, "src/file.txt")
            .expect_err("intermediate symlink must be rejected");
        let msg = format!("{:?}", err);
        assert!(msg.contains("symlink"), "expected symlink rejection, got: {msg}");
    }

    #[test]
    #[cfg(unix)]
    fn safe_join_accepts_normal_directory_chain() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path();
        std::fs::create_dir_all(project.join("a").join("b")).unwrap();

        // Real dirs, no symlinks → should pass
        let result = safe_join(project, "a/b/c.txt").unwrap();
        assert_eq!(result, project.join("a").join("b").join("c.txt"));
    }

    #[test]
    fn lineage_rejects_grandparent_not_direct_parent() {
        // Direct-parent-only: even if pushed appears deeper in lineage,
        // we can't observe that without recursive fetches. Reject.
        let snap = serde_json::json!({
            "project_manifest_hash": "manifest_z",
            "parent_hashes": ["intermediate_iii"],
        });
        // pushed_xxx is NOT in parent_hashes (would be parent's parent in
        // a real chain) — we have no way to verify, so reject.
        let err = verify_snapshot_lineage(&snap, "pushed_xxx", "result_zzz")
            .expect_err("non-direct-parent must reject");
        assert!(matches!(err, PullResultsError::UnrelatedSnapshot { .. }));
    }
}
