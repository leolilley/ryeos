//! Execution lifecycle: checkout, execute, fold-back.
//!
//! Manages the CAS-backed execution flow:
//! 1. Checkout project from CAS to working directory
//! 2. After execution, diff working dir and fold back changes

pub mod arch_check;
pub mod cache;
pub mod callback_token;
pub mod ingest;
pub mod launch;
pub mod launch_envelope;
pub mod lillux_bridge;
pub mod limits;
pub mod project_source;
pub mod runtime_dispatch;
pub mod runner;
pub mod thread_meta;

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;

use lillux::cas::{CasStore, sha256_hex};
use ryeos_state::signer::Signer;
use ryeos_state::objects::SourceManifest;

use self::cache::MaterializationCache;

// ── Checkout ────────────────────────────────────────────────────────

/// Checkout a project from CAS to a target directory.
///
/// Uses a 3-layer model matching the Python implementation:
/// 1. Check MaterializationCache (snapshot cache with `.snapshot_complete` marker)
/// 2. Materialize from CAS into cache with atomic staging
/// 3. Copy from cache to target (mutable execution space)
///
/// Returns the target directory path.
pub fn checkout_project(
    cas_root: &Path,
    manifest_hash: &str,
    target_dir: &Path,
    mat_cache: Option<&MaterializationCache>,
) -> Result<PathBuf> {
    let cas = CasStore::new(cas_root.to_path_buf());

    // Layer 1: Check snapshot cache
    if let Some(cache) = mat_cache {
        if cache.is_complete(manifest_hash) {
            let cached = cache.cache_dir(manifest_hash);
            copy_dir_recursive(&cached, target_dir)?;
            tracing::debug!(manifest_hash, "checkout from snapshot cache");
            return Ok(target_dir.to_path_buf());
        }
    }

    // Load manifest
    let manifest_obj = cas
        .get_object(manifest_hash)?
        .ok_or_else(|| anyhow::anyhow!("manifest {manifest_hash} not found"))?;

    let manifest = SourceManifest::from_value(&manifest_obj)?;

    // Determine materialization target: stage into cache if available, else direct
    let materialize_dir = if let Some(cache) = mat_cache {
        let staging = cache.cache_dir(&format!(
            "{manifest_hash}.staging.{}.{}",
            std::process::id(),
            rand::random::<u32>()
        ));
        fs::create_dir_all(&staging)?;
        staging
    } else {
        fs::create_dir_all(target_dir)?;
        target_dir.to_path_buf()
    };

    // Materialize item_source entries (items field)
    for (rel_path, object_hash) in &manifest.item_source_hashes {
        let target_path = materialize_dir.join(rel_path);
        if let Some(parent) = target_path.parent() {
            fs::create_dir_all(parent)?;
        }
        self::ingest::materialize_item(cas_root, object_hash, &target_path)?;
    }

    // Layer 2: Atomic cache promotion (staging → final cache dir)
    if let Some(cache) = mat_cache {
        let final_dir = cache.cache_dir(manifest_hash);
        if materialize_dir != target_dir.to_path_buf() {
            match fs::rename(&materialize_dir, &final_dir) {
                Ok(_) => {
                    cache.mark_complete(manifest_hash)?;
                    copy_dir_recursive(&final_dir, target_dir)?;
                }
                Err(_) => {
                    // Another process won the rename. Use their result if complete,
                    // otherwise fall back to our staging dir which is still intact.
                    if cache.is_complete(manifest_hash) {
                        copy_dir_recursive(&final_dir, target_dir)?;
                    } else {
                        copy_dir_recursive(&materialize_dir, target_dir)?;
                    }
                    let _ = fs::remove_dir_all(&materialize_dir);
                }
            }
        }
    }

    tracing::debug!(
        manifest_hash,
        item_source_hashes = manifest.item_source_hashes.len(),
        "checkout complete"
    );
    Ok(target_dir.to_path_buf())
}

// ── Fold-back ───────────────────────────────────────────────────────

/// Diff the working directory against the pre-execution manifest and
/// ingest new/changed files back into CAS.
///
/// Deletion-aware: files present in the pre-manifest but missing from
/// the working directory are removed from the new manifest.
///
/// Returns the new manifest hash if there were changes, or None if
/// the working directory is unchanged.
pub fn fold_back_outputs(
    cas_root: &Path,
    working_dir: &Path,
    pre_manifest_hash: &str,
) -> Result<Option<String>> {
    let cas = CasStore::new(cas_root.to_path_buf());

    // Load pre-execution manifest
    let pre_manifest_obj = cas
        .get_object(pre_manifest_hash)?
        .ok_or_else(|| anyhow::anyhow!("pre-manifest {pre_manifest_hash} not found"))?;
    let pre_manifest = SourceManifest::from_value(&pre_manifest_obj)?;

    // Build pre-execution integrity map: rel_path → integrity hash
    let mut pre_integrity: HashMap<String, String> = HashMap::new();
    for (rel_path, obj_hash) in &pre_manifest.item_source_hashes {
        if let Ok(Some(item_obj)) = cas.get_object(obj_hash) {
            if let Some(integrity) = item_obj.get("integrity").and_then(|v| v.as_str()) {
                pre_integrity.insert(rel_path.clone(), integrity.to_string());
            }
        }
    }

    // Walk working directory, find new/changed files
    // All changed/new files are ingested into `items` (the canonical format).
    let mut new_items: HashMap<String, String> = pre_manifest.item_source_hashes.clone();
    let mut changed = false;

    walk_and_diff(cas_root, working_dir, working_dir, &pre_integrity, &mut new_items, &mut changed)?;

    // Detect deletions: entries in pre-manifest but missing from working dir
    for rel_path in pre_manifest.item_source_hashes.keys() {
        let path = working_dir.join(rel_path);
        if !path.exists() || !path.is_file() {
            new_items.remove(rel_path);
            changed = true;
        }
    }

    if !changed {
        return Ok(None);
    }

    // Create new manifest
    let new_manifest = SourceManifest { item_source_hashes: new_items };
    let new_hash = cas.store_object(&new_manifest.to_value())?;

    tracing::debug!(
        old_hash = pre_manifest_hash,
        new_hash = %new_hash,
        "fold-back produced new manifest"
    );

    Ok(Some(new_hash))
}

/// Advance the project head ref after fold-back.
///
/// `current_snapshot_hash` must be the current HEAD snapshot hash (not a manifest hash).
/// Returns the new snapshot hash on success, or an error if the ref has moved.
pub fn advance_after_foldback(
    cas_root: &Path,
    refs_root: &Path,
    signer: &dyn Signer,
    project_path_hash: &str,
    new_manifest_hash: &str,
    current_snapshot_hash: &str,
) -> Result<String> {
    let cas = CasStore::new(cas_root.to_path_buf());
    let now = lillux::time::iso8601_now();
    let snapshot = ryeos_state::objects::ProjectSnapshot {
        project_manifest_hash: new_manifest_hash.to_string(),
        user_manifest_hash: None,
        parent_hashes: vec![current_snapshot_hash.to_string()],
        created_at: now,
        source: "fold-back".to_string(),
    };
    let new_snapshot_hash = cas.store_object(&snapshot.to_value())?;

    ryeos_state::refs::write_project_head_ref(
        refs_root,
        project_path_hash,
        &new_snapshot_hash,
        signer,
    )?;

    Ok(new_snapshot_hash)
}

// ── Helpers ─────────────────────────────────────────────────────────

fn walk_and_diff(
    cas_root: &Path,
    root: &Path,
    dir: &Path,
    pre_integrity: &HashMap<String, String>,
    items: &mut HashMap<String, String>,
    changed: &mut bool,
) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .to_string();

        if rel.starts_with("state/") || rel == "state" || rel.starts_with(".") {
            continue;
        }

        if path.is_dir() {
            walk_and_diff(cas_root, root, &path, pre_integrity, items, changed)?;
        } else if path.is_file() {
            let bytes = fs::read(&path)?;
            let integrity = sha256_hex(&bytes);

            // Check if file is new or changed
            match pre_integrity.get(&rel) {
                Some(old_integrity) if *old_integrity == integrity => {
                    // Unchanged — keep existing item
                }
                _ => {
                    // New or changed — ingest into items (canonical format).
                    let result: self::ingest::IngestResult = self::ingest::ingest_item(cas_root, &rel, &path)?;
                    tracing::trace!(
                        rel_path = %rel,
                        blob_hash = %result.blob_hash,
                        integrity = %result.integrity,
                        "ingested changed file"
                    );
                    items.insert(rel, result.object_hash);
                    *changed = true;
                }
            }
        }
    }
    Ok(())
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if src_path.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}
