//! Execution lifecycle: checkout, execute, fold-back.
//!
//! Manages the CAS-backed execution flow:
//! 1. Checkout project from CAS to working directory
//! 2. After execution, diff working dir and fold back changes

pub mod arch_check;
pub mod cache;
pub mod ingest;
pub mod launch;
pub mod launch_preparation;
pub(crate) mod launch_claim;
pub mod launch_envelope;
pub mod lillux_bridge;
pub mod limits;
pub(crate) mod process_attachment;
pub mod project_source;
pub mod runner;
pub mod runtime_dispatch;
pub mod spawn_detached_child;
pub mod spawn_follow_child;
pub mod thread_meta;

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use lillux::cas::sha256_hex;
use ryeos_state::objects::SourceManifest;
use ryeos_state::signer::Signer;

use self::cache::MaterializationCache;

/// A descriptor-pinned CAS publication whose immutable objects are protected
/// by durable recovery roots until a daemon-authoritative consumer is visible.
/// The recovery lease remains live across asynchronous launch; each synchronous
/// mutation phase acquires the shared guard before its write permit and holds
/// both through durable staged-root publication.
pub(crate) struct PendingCasPublication {
    authority: ryeos_state::PinnedStateAuthority,
    staged_roots: Option<ryeos_state::StagedCasRootLease>,
}

impl PendingCasPublication {
    fn publish(mut self) -> Result<()> {
        let guard = self.authority.acquire_shared_guard()?;
        self.authority.ensure_guard(&guard)?;
        self.staged_roots
            .as_mut()
            .expect("pending CAS publication always owns staged roots")
            .finish_admitted(&guard)?;
        self.staged_roots.take();
        Ok(())
    }
}

impl Drop for PendingCasPublication {
    fn drop(&mut self) {
        let Some(staged_roots) = self.staged_roots.as_mut() else {
            return;
        };
        match self.authority.acquire_shared_guard() {
            Ok(guard) => {
                if let Err(error) = staged_roots.finish_admitted(&guard) {
                    tracing::warn!(%error, "failed to discard staged CAS publication roots");
                }
            }
            Err(error) => {
                // Fail closed: keep the durable recovery roots and their lease
                // record for GC rather than falling back to a path-rebound
                // guard in `StagedCasRootLease::drop`.
                tracing::warn!(%error, "abandoning staged CAS roots under pinned-authority failure");
            }
        }
    }
}

pub(crate) struct PendingProjectManifest {
    pub hash: String,
    publication: PendingCasPublication,
}

pub(crate) struct PendingProjectSnapshot {
    pub hash: String,
    publication: PendingCasPublication,
}

impl PendingProjectSnapshot {
    pub fn publish(self) -> Result<()> {
        self.publication.publish()
    }
}

pub(crate) fn pinned_state_authority(
    state: &ryeos_app::state::AppState,
) -> Result<ryeos_state::PinnedStateAuthority> {
    state.state_store.with_state_db(|db| db.pinned_authority())
}

/// Capture a live project tree as an immutable CAS snapshot for durable
/// runtime reconstruction. The caller decides whether snapshot pinning is
/// required; once requested, any ingest/store failure is fail-closed.
pub(crate) fn capture_live_project_snapshot(
    state: &ryeos_app::state::AppState,
    project_path: &Path,
    source: &str,
) -> Result<PendingProjectSnapshot> {
    if !project_path.is_dir() {
        anyhow::bail!(
            "cannot snapshot missing project directory {}",
            project_path.display()
        );
    }
    let authority = pinned_state_authority(state)?;
    let guard = authority.acquire_shared_guard()?;
    authority.ensure_guard(&guard)?;
    let _permit = state
        .write_barrier
        .try_acquire()
        .map_err(|error| anyhow::anyhow!("cannot acquire CAS write permit: {error}"))?;
    let cas = authority.cas_store()?;
    let mut staged_roots = authority
        .require_recovery()?
        .begin_staged_cas_roots_admitted(&guard, source)?;
    let items = ingest::ingest_directory(
        &authority,
        &guard,
        &mut staged_roots,
        project_path,
        &state.ignore_matcher,
    )?;
    let manifest = SourceManifest {
        item_source_hashes: items,
    };
    let manifest_hash = staged_roots.store_object_admitted(&guard, &cas, &manifest.to_value())?;
    let hash = store_project_snapshot(&mut staged_roots, &guard, &cas, manifest_hash, source)?;
    Ok(PendingProjectSnapshot {
        hash,
        publication: PendingCasPublication {
            authority,
            staged_roots: Some(staged_roots),
        },
    })
}

/// Capture a live project source manifest under a durable recovery root. The
/// shared guard is acquired from the same descriptor-pinned authority before
/// the first blob write and remains held until the staged root is durable.
pub(crate) fn capture_live_project_manifest(
    state: &ryeos_app::state::AppState,
    project_path: &Path,
    source: &str,
) -> Result<PendingProjectManifest> {
    if !project_path.is_dir() {
        anyhow::bail!(
            "cannot snapshot missing project directory {}",
            project_path.display()
        );
    }
    let authority = pinned_state_authority(state)?;
    let guard = authority.acquire_shared_guard()?;
    authority.ensure_guard(&guard)?;
    let _permit = state
        .write_barrier
        .try_acquire()
        .map_err(|error| anyhow::anyhow!("cannot acquire CAS write permit: {error}"))?;
    let cas = authority.cas_store()?;
    let mut staged_roots = authority
        .require_recovery()?
        .begin_staged_cas_roots_admitted(&guard, source)?;
    let items = ingest::ingest_directory(
        &authority,
        &guard,
        &mut staged_roots,
        project_path,
        &state.ignore_matcher,
    )?;
    let manifest = SourceManifest {
        item_source_hashes: items,
    };
    let hash = staged_roots.store_object_admitted(&guard, &cas, &manifest.to_value())?;
    Ok(PendingProjectManifest {
        hash,
        publication: PendingCasPublication {
            authority,
            staged_roots: Some(staged_roots),
        },
    })
}

/// Promote an already-staged source manifest to a project snapshot under the
/// same pinned runtime/CAS/recovery authority and durable recovery lease.
pub(crate) fn capture_manifest_project_snapshot(
    state: &ryeos_app::state::AppState,
    manifest_hash: String,
    source: &str,
    mut publication: PendingCasPublication,
) -> Result<PendingProjectSnapshot> {
    let guard = publication.authority.acquire_shared_guard()?;
    publication.authority.ensure_guard(&guard)?;
    let _permit = state
        .write_barrier
        .try_acquire()
        .map_err(|error| anyhow::anyhow!("cannot acquire CAS write permit: {error}"))?;
    let cas = publication.authority.cas_store()?;
    if cas.get_object(&manifest_hash)?.is_none() {
        anyhow::bail!("project source manifest {manifest_hash} is no longer present in CAS");
    }
    publication
        .staged_roots
        .as_mut()
        .expect("pending CAS publication always owns staged roots")
        .protect_object_hash_admitted(&guard, &manifest_hash)?;
    let hash = store_project_snapshot(
        publication
            .staged_roots
            .as_mut()
            .expect("pending CAS publication always owns staged roots"),
        &guard,
        &cas,
        manifest_hash,
        source,
    )?;
    Ok(PendingProjectSnapshot { hash, publication })
}

fn store_project_snapshot(
    staged_roots: &mut ryeos_state::StagedCasRootLease,
    guard: &ryeos_state::CasMutationGuard,
    cas: &lillux::cas::CasStore,
    manifest_hash: String,
    source: &str,
) -> Result<String> {
    let snapshot = ryeos_state::objects::ProjectSnapshot {
        project_manifest_hash: manifest_hash,
        user_manifest_hash: None,
        message: None,
        project_sync_scope: ryeos_state::project_sync::ProjectSyncScope::FullProject,
        parent_hashes: Vec::new(),
        created_at: lillux::time::iso8601_now(),
        source: source.to_string(),
    };
    staged_roots.store_object_admitted(guard, cas, &snapshot.to_value())
}

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
    authority: &ryeos_state::PinnedStateAuthority,
    cas_mutation_guard: &ryeos_state::CasMutationGuard,
    manifest_hash: &str,
    target_dir: &Path,
    mat_cache: Option<&MaterializationCache>,
) -> Result<PathBuf> {
    authority.ensure_guard(cas_mutation_guard)?;
    let cas = authority.cas_store()?;

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
        self::ingest::materialize_item(authority, cas_mutation_guard, object_hash, &target_path)?;
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
    authority: &ryeos_state::PinnedStateAuthority,
    cas_mutation_guard: &ryeos_state::CasMutationGuard,
    working_dir: &Path,
    pre_manifest_hash: &str,
    ignore: &ryeos_app::ignore::IgnoreMatcher,
) -> Result<Option<String>> {
    authority.ensure_guard(cas_mutation_guard)?;
    let cas = authority.cas_store()?;

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

    walk_and_diff(
        authority,
        cas_mutation_guard,
        working_dir,
        working_dir,
        &pre_integrity,
        &mut new_items,
        &mut changed,
        ignore,
    )?;

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
    let new_manifest = SourceManifest {
        item_source_hashes: new_items,
    };
    let new_hash = cas.store_object(&new_manifest.to_value())?;

    tracing::debug!(
        old_hash = pre_manifest_hash,
        new_hash = %new_hash,
        "fold-back produced new manifest"
    );

    Ok(Some(new_hash))
}

/// Advance the principal-scoped project head ref after fold-back.
///
/// Uses compare-and-swap: `current_snapshot_hash` must match the
/// existing HEAD target, or the operation fails with a conflict error.
/// Returns the new snapshot hash on success.
///
/// The `principal_key` is the raw fingerprint hex (from
/// [`ryeos_state::refs::principal_storage_key`]).
pub fn advance_after_foldback(
    authority: &ryeos_state::PinnedStateAuthority,
    cas_mutation_guard: &ryeos_state::CasMutationGuard,
    state_db: &ryeos_state::StateDb,
    signer: &dyn Signer,
    principal_key: &str,
    project_path_hash: &str,
    new_manifest_hash: &str,
    current_snapshot_hash: &str,
) -> Result<String> {
    authority.ensure_guard(cas_mutation_guard)?;
    state_db
        .pinned_authority()?
        .ensure_guard(cas_mutation_guard)?;
    let cas = authority.cas_store()?;
    let now = lillux::time::iso8601_now();

    let current_snapshot_obj = cas.get_object(current_snapshot_hash)?.ok_or_else(|| {
        anyhow::anyhow!(
            "current snapshot {} not found in CAS",
            current_snapshot_hash
        )
    })?;
    let current_snapshot =
        ryeos_state::objects::ProjectSnapshot::from_value(&current_snapshot_obj)?;

    let snapshot = ryeos_state::objects::ProjectSnapshot {
        project_manifest_hash: new_manifest_hash.to_string(),
        user_manifest_hash: None,
        message: None,
        project_sync_scope: current_snapshot.project_sync_scope,
        parent_hashes: vec![current_snapshot_hash.to_string()],
        created_at: now,
        source: "fold-back".to_string(),
    };
    let new_snapshot_hash = cas.store_object(&snapshot.to_value())?;

    state_db.advance_project_head_ref(
        principal_key,
        project_path_hash,
        &new_snapshot_hash,
        current_snapshot_hash,
        signer,
        cas_mutation_guard,
    )?;

    Ok(new_snapshot_hash)
}

// ── Helpers ─────────────────────────────────────────────────────────

fn walk_and_diff(
    authority: &ryeos_state::PinnedStateAuthority,
    cas_mutation_guard: &ryeos_state::CasMutationGuard,
    root: &Path,
    dir: &Path,
    pre_integrity: &HashMap<String, String>,
    items: &mut HashMap<String, String>,
    changed: &mut bool,
    ignore: &ryeos_app::ignore::IgnoreMatcher,
) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let relative = path.strip_prefix(root).with_context(|| {
            format!(
                "fold-back path '{}' escaped project root '{}'",
                path.display(),
                root.display()
            )
        })?;
        let rel = relative
            .to_str()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "fold-back project-relative path '{}' is not valid UTF-8",
                    relative.display()
                )
            })?
            .replace('\\', "/");

        // Always skip state/ (internal daemon state)
        if rel.starts_with("state/") || rel == "state" {
            continue;
        }

        // Apply shared ignore rules
        if ignore.is_ignored(&rel) {
            continue;
        }

        if path.is_dir() {
            walk_and_diff(
                authority,
                cas_mutation_guard,
                root,
                &path,
                pre_integrity,
                items,
                changed,
                ignore,
            )?;
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
                    let result: self::ingest::IngestResult = self::ingest::ingest_item(
                        authority,
                        cas_mutation_guard,
                        None,
                        &rel,
                        &path,
                    )?;
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
