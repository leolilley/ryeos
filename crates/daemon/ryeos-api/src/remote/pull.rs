//! Client-side pull/apply pipeline for remote execution results.
//!
//! After `remote execute`, the remote node may have advanced its HEAD ref
//! (fold-back writes new files into CAS). This module fetches the resulting
//! snapshot, diffs against the exact tree that was pushed, and applies changes to
//! the local workspace with a clean-base conflict policy.

use std::io::{Read as _, Seek as _, Write as _};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use hmac::{Hmac, Mac as _};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::Sha256;

use lillux::cas::CasStore;
use ryeos_state::objects::{ProjectFile, ProjectSnapshot, ProjectSnapshotPolicy, ProjectTree};
use ryeos_state::{PinnedStateAuthority, StagedCasRootLease};

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
    #[error("remote result apply requires recovery: {0}")]
    RecoveryRequired(String),
    #[error("remote result rollback was incomplete: {0}")]
    RollbackIncomplete(String),
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
/// * `authority` — One descriptor-pinned local state authority captured by the
///   caller for the complete push/execute/pull operation.
/// * `pushed_snapshot_hash` — The snapshot we pushed (lineage anchor). The
///   result snapshot must equal this OR list it in `parent_hashes` (direct
///   parent only, per resolved design decision).
/// * `remote_snapshot_hash` — The snapshot hash from the remote execution result
/// * `local_project_root` — The local project directory to apply changes to.
///   `None` for `--no-project` mode skips diff/apply because no workspace
///   exists; snapshot lineage is still verified.
/// * `base_tree` — The exact tree that was pushed (from PushResult.tree)
pub async fn pull_results(
    client: &RemoteClient,
    authority: &PinnedStateAuthority,
    pushed_snapshot_hash: &str,
    remote_snapshot_hash: &str,
    local_project_root: Option<&Path>,
    base_tree: &ProjectTree,
) -> Result<PullResult, PullResultsError> {
    let local_cas = authority.cas_store().map_err(PullResultsError::Other)?;
    let recovery = authority
        .require_recovery()
        .map_err(PullResultsError::Other)?;
    let mut staged_roots = {
        let guard = authority
            .acquire_shared_guard()
            .map_err(PullResultsError::Other)?;
        recovery
            .begin_staged_cas_roots_admitted(&guard, "remote-pull-results")
            .map_err(PullResultsError::Other)?
    };

    let operation = pull_results_staged(
        client,
        authority,
        &local_cas,
        &mut staged_roots,
        pushed_snapshot_hash,
        remote_snapshot_hash,
        local_project_root,
        base_tree,
    )
    .await;

    finish_pull_staged_roots(authority, &mut staged_roots, operation)
}

// The staged pull transaction keeps its remote, authority, lease, and three
// snapshot identities explicit so none can be silently substituted.
#[allow(clippy::too_many_arguments)]
async fn pull_results_staged(
    client: &RemoteClient,
    authority: &PinnedStateAuthority,
    local_cas: &CasStore,
    staged_roots: &mut StagedCasRootLease,
    pushed_snapshot_hash: &str,
    remote_snapshot_hash: &str,
    local_project_root: Option<&Path>,
    base_tree: &ProjectTree,
) -> Result<PullResult, PullResultsError> {
    // 1. Fetch remote snapshot object
    let snapshot_objs = client
        .objects_get(&[remote_snapshot_hash.to_string()], &[])
        .await
        .map_err(PullResultsError::Other)?;

    let snapshot_val = snapshot_objs
        .find_object(remote_snapshot_hash)
        .ok_or_else(|| {
            PullResultsError::InvalidRemoteSnapshot(format!(
                "snapshot {} not found in objects_get response",
                remote_snapshot_hash
            ))
        })?;
    store_remote_object(
        authority,
        staged_roots,
        local_cas,
        remote_snapshot_hash,
        &snapshot_val,
    )?;

    // 1a. Lineage check. Runs in every mode — including --no-project —
    //     so a misconfigured / hostile remote can't slip an unrelated
    //     snapshot past us just because there's no local project to
    //     diff against.
    verify_snapshot_lineage(&snapshot_val, pushed_snapshot_hash, remote_snapshot_hash)?;

    // Fetch and validate the complete result-generation closure before any
    // local mutation. This includes unchanged ProjectFile objects and the
    // exact stored policy; a destination cannot reinterpret the tree using
    // its current ignore configuration.
    let mut fetched_count = 1usize;
    let snapshot = ProjectSnapshot::from_value(&snapshot_val)
        .map_err(|error| PullResultsError::InvalidRemoteSnapshot(error.to_string()))?;
    let pushed_snapshot_value = local_cas.get_object(pushed_snapshot_hash)?.ok_or_else(|| {
        PullResultsError::InvalidRemoteSnapshot(format!(
            "locally pushed snapshot {pushed_snapshot_hash} is absent"
        ))
    })?;
    let pushed_snapshot = ProjectSnapshot::from_value(&pushed_snapshot_value)
        .map_err(|error| PullResultsError::InvalidRemoteSnapshot(error.to_string()))?;
    if snapshot.effective_policy_hash != pushed_snapshot.effective_policy_hash {
        return Err(PullResultsError::InvalidRemoteSnapshot(format!(
            "result snapshot changed project policy from {} to {} without an authorized policy transition",
            pushed_snapshot.effective_policy_hash, snapshot.effective_policy_hash
        )));
    }
    let roots = [
        snapshot.project_tree_hash.clone(),
        snapshot.effective_policy_hash.clone(),
    ];
    let roots_response = client
        .objects_get(&roots, &[])
        .await
        .map_err(PullResultsError::Other)?;
    let tree_val = roots_response
        .find_object(&snapshot.project_tree_hash)
        .ok_or_else(|| {
            PullResultsError::InvalidRemoteSnapshot(format!(
                "project tree {} is absent",
                snapshot.project_tree_hash
            ))
        })?;
    let policy_val = roots_response
        .find_object(&snapshot.effective_policy_hash)
        .ok_or_else(|| {
            PullResultsError::InvalidRemoteSnapshot(format!(
                "project policy {} is absent",
                snapshot.effective_policy_hash
            ))
        })?;
    store_remote_object(
        authority,
        staged_roots,
        local_cas,
        &snapshot.project_tree_hash,
        &tree_val,
    )?;
    store_remote_object(
        authority,
        staged_roots,
        local_cas,
        &snapshot.effective_policy_hash,
        &policy_val,
    )?;
    fetched_count += 2;
    let remote_tree = ProjectTree::from_value(&tree_val)
        .map_err(|error| PullResultsError::InvalidRemoteSnapshot(error.to_string()))?;
    let policy = ProjectSnapshotPolicy::from_value(&policy_val)
        .map_err(|error| PullResultsError::InvalidRemoteSnapshot(error.to_string()))?;
    ryeos_state::project_sync::validate_project_tree_paths(&remote_tree, &policy)
        .map_err(|error| PullResultsError::InvalidRemoteSnapshot(error.to_string()))?;
    ryeos_state::project_sync::validate_captured_policy_source(local_cas, &remote_tree, &policy)
        .map_err(|error| PullResultsError::InvalidRemoteSnapshot(error.to_string()))?;

    let limits = ryeos_state::object_closure::ObjectClosureLimits::for_project_snapshot_transport();
    let mut project_files = Vec::with_capacity(remote_tree.files.len());
    for object_hash in remote_tree.files.values() {
        let value = match local_cas.get_object(object_hash)? {
            Some(value) => value,
            None => {
                let response = client
                    .objects_get(std::slice::from_ref(object_hash), &[])
                    .await
                    .map_err(PullResultsError::Other)?;
                let value = response.find_object(object_hash).ok_or_else(|| {
                    PullResultsError::InvalidRemoteSnapshot(format!(
                        "project file object {object_hash} is absent"
                    ))
                })?;
                store_remote_object(authority, staged_roots, local_cas, object_hash, &value)?;
                fetched_count += 1;
                value
            }
        };
        let file = ProjectFile::from_value(&value)
            .map_err(|error| PullResultsError::InvalidRemoteSnapshot(error.to_string()))?;
        project_files.push(file);
    }
    for file in &project_files {
        if local_cas.has_blob(&file.blob_hash)? {
            continue;
        }
        fetch_remote_blob_streaming(client, authority, staged_roots, local_cas, file).await?;
        fetched_count += 1;
    }
    let closure = ryeos_state::object_closure::collect_object_closure_with_cas_and_limits(
        local_cas,
        [remote_snapshot_hash.to_string()],
        limits,
    )?;
    if !closure.is_complete() {
        return Err(PullResultsError::InvalidRemoteSnapshot(format!(
            "result generation closure is incomplete: {closure:?}"
        )));
    }
    {
        let guard = authority.acquire_shared_guard()?;
        staged_roots.protect_cas_closure_admitted(
            &guard,
            closure.object_hashes.iter().map(String::as_str),
            closure.blob_hashes.iter().map(String::as_str),
        )?;
    }

    let (files_updated, files_deleted) = match local_project_root {
        Some(root) => {
            let journal_auth_key = authority
                .require_recovery()?
                .remote_pull_journal_auth_key()?;
            apply_tree_diff(local_cas, root, base_tree, &remote_tree, &journal_auth_key)?
        }
        None => (0, 0),
    };

    Ok(PullResult {
        snapshot_hash: remote_snapshot_hash.to_string(),
        cas_objects_fetched: fetched_count,
        files_updated,
        files_deleted,
    })
}

fn store_remote_object(
    authority: &PinnedStateAuthority,
    staged_roots: &mut StagedCasRootLease,
    local_cas: &CasStore,
    expected_hash: &str,
    value: &Value,
) -> Result<(), PullResultsError> {
    let guard = authority
        .acquire_shared_guard()
        .map_err(PullResultsError::Other)?;
    let stored = staged_roots
        .store_object_admitted(&guard, local_cas, value)
        .map_err(PullResultsError::Other)?;
    if stored != expected_hash {
        return Err(PullResultsError::InvalidRemoteSnapshot(format!(
            "object hash mismatch: expected {expected_hash}, got {stored}"
        )));
    }
    Ok(())
}

async fn fetch_remote_blob_streaming(
    client: &RemoteClient,
    authority: &PinnedStateAuthority,
    staged_roots: &mut StagedCasRootLease,
    local_cas: &CasStore,
    file: &ProjectFile,
) -> Result<(), PullResultsError> {
    let mut part = staged_roots.open_blob_part(&file.blob_hash)?;
    {
        let destination = part.file_mut();
        let mut offset = 0_u64;
        while offset < file.size {
            let chunk = client
                .objects_get_blob_chunk(
                    &file.blob_hash,
                    offset,
                    crate::handlers::objects_get::MAX_BLOB_CHUNK_BYTES,
                )
                .await?;
            if chunk.kind == "missing" {
                anyhow::bail!("project blob {} is absent", file.blob_hash);
            }
            if chunk.total_size != Some(file.size) {
                anyhow::bail!(
                    "project blob {} changed its declared size during transfer",
                    file.blob_hash
                );
            }
            let bytes = chunk.decoded_data()?;
            if bytes.is_empty() {
                anyhow::bail!("project blob transfer made no progress");
            }
            destination.write_all(&bytes)?;
            offset = offset
                .checked_add(u64::try_from(bytes.len())?)
                .ok_or_else(|| anyhow::anyhow!("project blob transfer size overflow"))?;
            if chunk.eof != Some(offset == file.size) {
                anyhow::bail!("project blob transfer returned a contradictory EOF marker");
            }
        }
    }
    let guard = authority.acquire_shared_guard()?;
    staged_roots
        .publish_blob_part_admitted(&guard, local_cas, part, &file.blob_hash, file.size)
        .map_err(PullResultsError::Other)
}

fn finish_pull_staged_roots<T>(
    authority: &PinnedStateAuthority,
    staged_roots: &mut StagedCasRootLease,
    operation: Result<T, PullResultsError>,
) -> Result<T, PullResultsError> {
    let guard = authority
        .acquire_shared_guard()
        .map_err(PullResultsError::Other)?;
    let finish = staged_roots
        .finish_admitted(&guard)
        .map_err(PullResultsError::Other);
    match (operation, finish) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error),
    }
}

/// A planned change to apply.
enum PlannedChange {
    /// Write new content to this relative path.
    Write {
        rel_path: String,
        blob_hash: String,
        normalized_mode: u32,
        expected_live: Option<std::fs::File>,
        expected_blob_hash: Option<String>,
        expected_mode: Option<u32>,
    },
    /// Delete this relative path.
    Delete {
        rel_path: String,
        expected_live: std::fs::File,
        expected_blob_hash: String,
        expected_mode: u32,
    },
}

struct AppliedWrite {
    rel_path: String,
    installed: std::fs::File,
}

const PULL_RECOVERY_JOURNAL_KIND: &str = "ryeos_remote_result_apply_recovery";
const PULL_RECOVERY_JOURNAL_SCHEMA: u32 = 2;
// A transported ProjectTree may be 32 MiB. Recovery adds one base/result
// identity per changed entry, so keep a separate bounded envelope large enough
// for every accepted tree rather than making valid large diffs unrecoverable.
const MAX_PULL_RECOVERY_JOURNAL_BYTES: usize = 64 * 1024 * 1024;
const MAX_PULL_RECOVERY_CHANGES: usize = 100_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecoveryFileIdentity {
    blob_hash: String,
    normalized_mode: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
enum PullRecoveryChange {
    Write {
        path: String,
        artifact: String,
        base: Option<RecoveryFileIdentity>,
        result: RecoveryFileIdentity,
    },
    Delete {
        path: String,
        artifact: String,
        base: RecoveryFileIdentity,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PullRecoveryJournal {
    kind: String,
    schema: u32,
    transaction_id: String,
    project_root: String,
    changes: Vec<PullRecoveryChange>,
    auth_tag: String,
}

impl PullRecoveryJournal {
    fn authenticated_value(&self) -> Value {
        serde_json::json!({
            "kind": self.kind,
            "schema": self.schema,
            "transaction_id": self.transaction_id,
            "project_root": self.project_root,
            "changes": self.changes,
        })
    }
}

/// Apply the diff between base and remote project trees to the local workspace.
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
fn apply_tree_diff(
    local_cas: &CasStore,
    local_project_root: &Path,
    base_tree: &ProjectTree,
    remote_tree: &ProjectTree,
    journal_auth_key: &[u8; 32],
) -> Result<(usize, usize), PullResultsError> {
    let project_root = lillux::PinnedDirectory::open(local_project_root)?
        .ok_or_else(|| anyhow::anyhow!("local project root disappeared"))?;
    let _pull_lock = acquire_pull_apply_lock(&project_root)?;
    if let Some(report) =
        recover_interrupted_pull_apply(&project_root, local_cas, journal_auth_key)?
    {
        return Err(PullResultsError::RecoveryRequired(report));
    }
    ensure_no_pull_recovery_artifacts(&project_root)?;
    // Collect all paths from base + remote trees.
    let mut all_paths: std::collections::HashSet<String> = std::collections::HashSet::new();
    for path in base_tree.files.keys() {
        all_paths.insert(path.clone());
    }
    for path in remote_tree.files.keys() {
        all_paths.insert(path.clone());
    }

    // Phase A: validate — check all tracked paths for drift, and all new
    // paths for pre-existing files. Also resolve all CAS content.
    let mut planned: Vec<PlannedChange> = Vec::new();

    for path in &all_paths {
        let base_hash = base_tree.files.get(path);
        let remote_hash = remote_tree.files.get(path);
        ryeos_state::project_sync::validate_safe_relative_path(path)
            .map_err(PullResultsError::Other)?;
        let local_file = open_relative_regular(&project_root, path)?;
        let base_file = base_hash
            .map(|hash| load_project_file(local_cas, hash))
            .transpose()?;

        // If the path was in the base, verify local hasn't drifted
        if let Some(base_file) = base_file.as_ref() {
            let (local_hash, local_mode) = match local_file.as_ref() {
                Some(file) => (
                    Some(hash_open_regular(
                        file.try_clone().map_err(anyhow::Error::from)?,
                    )?),
                    Some(normalized_open_mode(file)?),
                ),
                None => (None, None),
            };
            if local_hash.as_deref() != Some(base_file.blob_hash.as_str())
                || local_mode != Some(base_file.normalized_mode)
            {
                return Err(PullResultsError::LocalConflict(path.clone()));
            }
        }

        // If the path is NEW in remote (not in base), check there's no
        // pre-existing local file — that would be a conflict too.
        if base_hash.is_none() && remote_hash.is_some() && local_file.is_some() {
            return Err(PullResultsError::LocalConflict(format!(
                "{} (new remote file would overwrite local)",
                path
            )));
        }

        // Plan the change
        match (base_hash, remote_hash) {
            // Unchanged — no-op
            (Some(b), Some(r)) if b == r => {}
            // File changed — resolve content from CAS (fail hard if missing)
            (Some(_), Some(r)) => {
                let file = require_project_file_blob(local_cas, r, path)?;
                planned.push(PlannedChange::Write {
                    rel_path: path.clone(),
                    blob_hash: file.blob_hash,
                    normalized_mode: file.normalized_mode,
                    expected_live: local_file,
                    expected_blob_hash: Some(
                        base_file
                            .as_ref()
                            .expect("changed base file was loaded")
                            .blob_hash
                            .clone(),
                    ),
                    expected_mode: Some(
                        base_file
                            .as_ref()
                            .expect("changed base file was loaded")
                            .normalized_mode,
                    ),
                });
            }
            // File deleted on remote
            (Some(_), None) => {
                if local_file.is_some() {
                    planned.push(PlannedChange::Delete {
                        rel_path: path.clone(),
                        expected_live: local_file.expect("base file was verified above"),
                        expected_blob_hash: base_file
                            .as_ref()
                            .expect("deleted base file was loaded")
                            .blob_hash
                            .clone(),
                        expected_mode: base_file
                            .as_ref()
                            .expect("deleted base file was loaded")
                            .normalized_mode,
                    });
                }
            }
            // New file on remote — resolve content from CAS (fail hard if missing)
            (None, Some(r)) => {
                let file = require_project_file_blob(local_cas, r, path)?;
                planned.push(PlannedChange::Write {
                    rel_path: path.clone(),
                    blob_hash: file.blob_hash,
                    normalized_mode: file.normalized_mode,
                    expected_live: None,
                    expected_blob_hash: None,
                    expected_mode: None,
                });
            }
            // Both None — shouldn't happen but no-op
            (None, None) => {}
        }
    }

    // Phase B: all content resolved successfully. Stage writes into a
    // temp directory. This is the only phase that touches the filesystem
    // before Phase C — if anything fails here the workspace is untouched.
    let transaction_id = format!("{:032x}", rand::random::<u128>());
    let mut backup = PinnedScratchDirectory::create(
        &project_root,
        &format!(".ryeos-pull-backup-{transaction_id}"),
    )?;
    write_pull_recovery_journal(
        &project_root,
        backup.directory(),
        &transaction_id,
        journal_auth_key,
        &planned,
    )?;
    let mut staging = PinnedScratchDirectory::create(
        &project_root,
        &format!(".ryeos-pull-staging-{transaction_id}"),
    )?;

    for change in &planned {
        if let PlannedChange::Write {
            rel_path,
            blob_hash,
            normalized_mode,
            ..
        } = change
        {
            let artifact = scratch_artifact_name(rel_path);
            let staged_path = staging.directory().descriptor_child_path(&artifact)?;
            local_cas
                .materialize_blob_to_new_file(blob_hash, &staged_path, *normalized_mode)
                .with_context(|| format!("stage file {}", rel_path))
                .map_err(PullResultsError::Other)?;
        }
    }
    // Phase C: atomic swap. We backup originals to `.ryeos-pull-backup/`,
    // then swap staged files into place and perform deletes. If anything
    // fails mid-swap, we roll back from the backup so the workspace is
    // never left in a partial state.
    let mut files_updated = 0usize;
    let mut files_deleted = 0usize;

    // C.1: Backup any live files that will be overwritten or deleted.
    for change in &planned {
        match change {
            PlannedChange::Write {
                rel_path,
                expected_live,
                expected_blob_hash,
                expected_mode,
                ..
            } => {
                revalidate_expected_live(
                    &project_root,
                    rel_path,
                    expected_live.as_ref(),
                    expected_blob_hash.as_deref(),
                    *expected_mode,
                )?;
                if expected_live.is_some() {
                    let artifact = scratch_artifact_name(rel_path);
                    let backup_path = backup.directory().descriptor_child_path(&artifact)?;
                    local_cas.materialize_blob_to_new_file(
                        expected_blob_hash
                            .as_deref()
                            .expect("existing write has a base blob"),
                        &backup_path,
                        expected_mode.expect("existing write has a base mode"),
                    )?;
                }
            }
            PlannedChange::Delete {
                rel_path,
                expected_live,
                expected_blob_hash,
                expected_mode,
            } => {
                revalidate_expected_live(
                    &project_root,
                    rel_path,
                    Some(expected_live),
                    Some(expected_blob_hash),
                    Some(*expected_mode),
                )?;
                let artifact = scratch_artifact_name(rel_path);
                let backup_path = backup.directory().descriptor_child_path(&artifact)?;
                local_cas.materialize_blob_to_new_file(
                    expected_blob_hash,
                    &backup_path,
                    *expected_mode,
                )?;
            }
        }
    }

    // C.2: Apply writes and deletes. On any failure, roll back.
    let apply_result = apply_with_rollback(
        &planned,
        &project_root,
        staging.directory(),
        backup.directory(),
        &mut files_updated,
        &mut files_deleted,
    );

    if matches!(
        &apply_result,
        Err(PullResultsError::RecoveryRequired(_) | PullResultsError::RollbackIncomplete(_))
    ) {
        staging.preserve();
        backup.preserve();
    }
    apply_result
}

/// Apply planned writes and deletes with rollback on failure.
///
/// If any individual operation fails, restores all previously-applied
/// changes from the backup directory so the workspace is left in its
/// original state.
fn apply_with_rollback(
    planned: &[PlannedChange],
    project_root: &lillux::PinnedDirectory,
    staging: &lillux::PinnedDirectory,
    backup: &lillux::PinnedDirectory,
    files_updated: &mut usize,
    files_deleted: &mut usize,
) -> Result<(usize, usize), PullResultsError> {
    // Track what we've applied so far for rollback.
    let mut applied_writes: Vec<AppliedWrite> = Vec::new();
    let mut applied_deletes: Vec<String> = Vec::new();
    let mut namespace_recovery_required = false;

    for change in planned {
        enum AppliedChange {
            Write(String, std::fs::File),
            Delete(String),
        }
        let applied = (|| -> Result<AppliedChange, PullResultsError> {
            match change {
                PlannedChange::Write {
                    rel_path,
                    expected_live,
                    expected_blob_hash,
                    expected_mode,
                    ..
                } => {
                    revalidate_expected_live(
                        project_root,
                        rel_path,
                        expected_live.as_ref(),
                        expected_blob_hash.as_deref(),
                        *expected_mode,
                    )?;
                    let (live_parent, live_name) =
                        pinned_relative_parent(project_root, rel_path, true)?
                            .ok_or_else(|| anyhow::anyhow!("live parent could not be created"))?;
                    revalidate_expected_entry(
                        &live_parent,
                        &live_name,
                        rel_path,
                        expected_live.as_ref(),
                        expected_blob_hash.as_deref(),
                        *expected_mode,
                    )?;
                    let artifact = scratch_artifact_name(rel_path);
                    let staged = staging
                        .open_regular(&artifact, false)?
                        .ok_or_else(|| anyhow::anyhow!("staged remote result disappeared"))?;
                    if let Err(error) = live_parent.replace_regular_from_if_matches_atomic(
                        &live_name,
                        expected_live.as_ref(),
                        |expected| {
                            validate_expected_content(
                                expected,
                                rel_path,
                                expected_blob_hash.as_deref(),
                                *expected_mode,
                            )
                        },
                        staging,
                        &artifact,
                        &staged,
                    ) {
                        namespace_recovery_required |= error.namespace_requires_recovery();
                        if error.namespace_committed() {
                            applied_writes.push(AppliedWrite {
                                rel_path: rel_path.clone(),
                                installed: staged,
                            });
                        }
                        return Err(PullResultsError::Other(error.into()));
                    }
                    if let Err(error) = live_parent.ensure_path_binding() {
                        applied_writes.push(AppliedWrite {
                            rel_path: rel_path.clone(),
                            installed: staged,
                        });
                        return Err(PullResultsError::Other(
                            error.context("remote result write committed through a rebound parent"),
                        ));
                    }
                    Ok(AppliedChange::Write(rel_path.clone(), staged))
                }
                PlannedChange::Delete {
                    rel_path,
                    expected_live,
                    expected_blob_hash,
                    expected_mode,
                } => {
                    let (live_parent, live_name) =
                        pinned_relative_parent(project_root, rel_path, false)?
                            .ok_or_else(|| anyhow::anyhow!("live delete parent disappeared"))?;
                    revalidate_expected_entry(
                        &live_parent,
                        &live_name,
                        rel_path,
                        Some(expected_live),
                        Some(expected_blob_hash),
                        Some(*expected_mode),
                    )?;
                    if let Err(error) = live_parent.remove_if_same_validated_atomic(
                        &live_name,
                        expected_live,
                        |expected| {
                            validate_expected_content(
                                expected,
                                rel_path,
                                Some(expected_blob_hash),
                                Some(*expected_mode),
                            )
                        },
                    ) {
                        namespace_recovery_required |= error.namespace_requires_recovery();
                        if error.namespace_committed() {
                            applied_deletes.push(rel_path.clone());
                        }
                        return Err(PullResultsError::Other(error.into()));
                    }
                    if let Err(error) = live_parent.ensure_path_binding() {
                        applied_deletes.push(rel_path.clone());
                        return Err(PullResultsError::Other(error.context(
                            "remote result delete committed through a rebound parent",
                        )));
                    }
                    Ok(AppliedChange::Delete(rel_path.clone()))
                }
            }
        })();
        match applied {
            Ok(AppliedChange::Write(path, installed)) => {
                applied_writes.push(AppliedWrite {
                    rel_path: path,
                    installed,
                });
                *files_updated += 1;
            }
            Ok(AppliedChange::Delete(path)) => {
                applied_deletes.push(path);
                *files_deleted += 1;
            }
            Err(error) => {
                rollback_pinned_changes(&applied_writes, &applied_deletes, project_root, backup)?;
                if namespace_recovery_required {
                    return Err(PullResultsError::RecoveryRequired(error.to_string()));
                }
                return Err(error);
            }
        }
    }

    if let Err(error) = project_root.ensure_path_binding() {
        rollback_pinned_changes(&applied_writes, &applied_deletes, project_root, backup)?;
        return Err(PullResultsError::Other(error.context(
            "local project root was rebound during remote result apply",
        )));
    }

    Ok((*files_updated, *files_deleted))
}

fn rollback_pinned_changes(
    applied_writes: &[AppliedWrite],
    applied_deletes: &[String],
    project_root: &lillux::PinnedDirectory,
    backup: &lillux::PinnedDirectory,
) -> Result<(), PullResultsError> {
    let mut errors = Vec::new();
    for applied in applied_writes.iter().rev() {
        let rel_path = &applied.rel_path;
        let rollback = (|| -> Result<()> {
            let (live_parent, live_name) = pinned_relative_parent(project_root, rel_path, true)?
                .ok_or_else(|| anyhow::anyhow!("rollback parent could not be opened"))?;
            live_parent.ensure_path_binding()?;
            let artifact = scratch_artifact_name(rel_path);
            if let Some(saved) = backup.open_regular(&artifact, false)? {
                live_parent
                    .replace_regular_from_if_matches_atomic(
                        &live_name,
                        Some(&applied.installed),
                        |_| Ok(()),
                        backup,
                        &artifact,
                        &saved,
                    )
                    .map_err(anyhow::Error::from)?;
            } else {
                live_parent
                    .remove_if_same_atomic(&live_name, &applied.installed)
                    .map_err(anyhow::Error::from)?;
            }
            Ok(())
        })();
        if let Err(error) = rollback {
            errors.push(format!("restore write {rel_path}: {error:#}"));
        }
    }
    for rel_path in applied_deletes.iter().rev() {
        let rollback = (|| -> Result<()> {
            let (live_parent, live_name) = pinned_relative_parent(project_root, rel_path, true)?
                .ok_or_else(|| anyhow::anyhow!("rollback parent could not be opened"))?;
            live_parent.ensure_path_binding()?;
            let artifact = scratch_artifact_name(rel_path);
            let saved = backup
                .open_regular(&artifact, false)?
                .ok_or_else(|| anyhow::anyhow!("rollback backup disappeared"))?;
            live_parent
                .replace_regular_from_if_matches_atomic(
                    &live_name,
                    None,
                    |_| Ok(()),
                    backup,
                    &artifact,
                    &saved,
                )
                .map_err(anyhow::Error::from)?;
            Ok(())
        })();
        if let Err(error) = rollback {
            errors.push(format!("restore delete {rel_path}: {error:#}"));
        }
    }
    if !errors.is_empty() {
        return Err(PullResultsError::RollbackIncomplete(errors.join("; ")));
    }
    Ok(())
}

fn pinned_relative_parent(
    root: &lillux::PinnedDirectory,
    relative: &str,
    create: bool,
) -> Result<Option<(lillux::PinnedDirectory, std::ffi::OsString)>, PullResultsError> {
    use std::path::Component;

    ryeos_state::project_sync::validate_safe_relative_path(relative)
        .map_err(PullResultsError::Other)?;
    let mut parent = root.try_clone()?;
    let mut components = Path::new(relative).components().peekable();
    while let Some(component) = components.next() {
        let Component::Normal(name) = component else {
            return Err(PullResultsError::Other(anyhow::anyhow!(
                "remote project path is not normalized: {relative}"
            )));
        };
        if components.peek().is_none() {
            return Ok(Some((parent, name.to_os_string())));
        }
        parent = if create {
            parent.open_or_create_child(name, 0o700)?
        } else {
            let Some(child) = parent.open_child_directory(name)? else {
                return Ok(None);
            };
            child
        };
    }
    Ok(None)
}

fn open_relative_regular(
    root: &lillux::PinnedDirectory,
    relative: &str,
) -> Result<Option<std::fs::File>, PullResultsError> {
    let Some((parent, name)) = pinned_relative_parent(root, relative, false)? else {
        return Ok(None);
    };
    parent.open_regular(&name, false).map_err(Into::into)
}

fn revalidate_expected_live(
    root: &lillux::PinnedDirectory,
    relative: &str,
    expected: Option<&std::fs::File>,
    expected_blob_hash: Option<&str>,
    expected_mode: Option<u32>,
) -> Result<(), PullResultsError> {
    let Some((parent, name)) = pinned_relative_parent(root, relative, false)? else {
        return if expected.is_none() {
            Ok(())
        } else {
            Err(PullResultsError::LocalConflict(relative.to_owned()))
        };
    };
    revalidate_expected_entry(
        &parent,
        &name,
        relative,
        expected,
        expected_blob_hash,
        expected_mode,
    )
}

fn revalidate_expected_entry(
    parent: &lillux::PinnedDirectory,
    name: &std::ffi::OsStr,
    relative: &str,
    expected: Option<&std::fs::File>,
    expected_blob_hash: Option<&str>,
    expected_mode: Option<u32>,
) -> Result<(), PullResultsError> {
    parent
        .ensure_path_binding()
        .map_err(|_| PullResultsError::LocalConflict(relative.to_owned()))?;
    parent
        .ensure_regular_entry_matches(name, expected)
        .map_err(|_| PullResultsError::LocalConflict(relative.to_owned()))?;
    let Some(expected) = expected else {
        if expected_blob_hash.is_some() || expected_mode.is_some() {
            return Err(PullResultsError::Other(anyhow::anyhow!(
                "missing clean-base descriptor carried expected metadata"
            )));
        }
        return Ok(());
    };
    validate_expected_content(expected, relative, expected_blob_hash, expected_mode)
        .map_err(PullResultsError::Other)
}

fn validate_expected_content(
    expected: &std::fs::File,
    relative: &str,
    expected_blob_hash: Option<&str>,
    expected_mode: Option<u32>,
) -> Result<()> {
    let observed_hash = hash_open_regular(expected.try_clone().map_err(anyhow::Error::from)?)
        .map_err(anyhow::Error::new)?;
    let observed_mode = normalized_open_mode(expected).map_err(anyhow::Error::new)?;
    if expected_blob_hash != Some(observed_hash.as_str()) || expected_mode != Some(observed_mode) {
        anyhow::bail!("local workspace conflict at {relative}");
    }
    Ok(())
}

fn hash_open_regular(mut source: std::fs::File) -> Result<String, PullResultsError> {
    use sha2::Digest as _;
    source
        .seek(std::io::SeekFrom::Start(0))
        .map_err(anyhow::Error::from)?;
    let before = source.metadata().map_err(anyhow::Error::from)?;
    let mut digest = sha2::Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let read = source.read(&mut buffer).map_err(anyhow::Error::from)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    let after = source.metadata().map_err(anyhow::Error::from)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if before.dev() != after.dev()
            || before.ino() != after.ino()
            || before.size() != after.size()
            || before.mtime() != after.mtime()
            || before.mtime_nsec() != after.mtime_nsec()
            || before.ctime() != after.ctime()
            || before.ctime_nsec() != after.ctime_nsec()
        {
            return Err(PullResultsError::LocalConflict(
                "file changed while hashing".to_owned(),
            ));
        }
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn normalized_open_mode(file: &std::fs::File) -> Result<u32, PullResultsError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        Ok(ProjectFile::normalize_mode(
            file.metadata()
                .map_err(anyhow::Error::from)?
                .permissions()
                .mode(),
        ))
    }
    #[cfg(not(unix))]
    {
        let _ = file;
        Ok(ProjectFile::REGULAR_MODE)
    }
}

fn scratch_artifact_name(relative: &str) -> std::ffi::OsString {
    std::ffi::OsString::from(lillux::sha256_hex(relative.as_bytes()))
}

fn write_pull_recovery_journal(
    project_root: &lillux::PinnedDirectory,
    backup: &lillux::PinnedDirectory,
    transaction_id: &str,
    auth_key: &[u8; 32],
    planned: &[PlannedChange],
) -> Result<(), PullResultsError> {
    if planned.len() > MAX_PULL_RECOVERY_CHANGES {
        return Err(PullResultsError::Other(anyhow::anyhow!(
            "remote result apply has {} changes, exceeding the recoverable journal limit of {MAX_PULL_RECOVERY_CHANGES}",
            planned.len()
        )));
    }
    let changes = planned
        .iter()
        .map(|change| match change {
            PlannedChange::Write {
                rel_path,
                blob_hash,
                normalized_mode,
                expected_blob_hash,
                expected_mode,
                ..
            } => PullRecoveryChange::Write {
                path: rel_path.clone(),
                artifact: scratch_artifact_name(rel_path)
                    .to_string_lossy()
                    .into_owned(),
                base: expected_blob_hash.as_ref().zip(*expected_mode).map(
                    |(blob_hash, normalized_mode)| RecoveryFileIdentity {
                        blob_hash: blob_hash.clone(),
                        normalized_mode,
                    },
                ),
                result: RecoveryFileIdentity {
                    blob_hash: blob_hash.clone(),
                    normalized_mode: *normalized_mode,
                },
            },
            PlannedChange::Delete {
                rel_path,
                expected_blob_hash,
                expected_mode,
                ..
            } => PullRecoveryChange::Delete {
                path: rel_path.clone(),
                artifact: scratch_artifact_name(rel_path)
                    .to_string_lossy()
                    .into_owned(),
                base: RecoveryFileIdentity {
                    blob_hash: expected_blob_hash.clone(),
                    normalized_mode: *expected_mode,
                },
            },
        })
        .collect::<Vec<_>>();
    let project_root_path = project_root
        .path()
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("project root path is not UTF-8"))?
        .to_owned();
    let mut journal = PullRecoveryJournal {
        kind: PULL_RECOVERY_JOURNAL_KIND.to_owned(),
        schema: PULL_RECOVERY_JOURNAL_SCHEMA,
        transaction_id: transaction_id.to_owned(),
        project_root: project_root_path,
        changes,
        auth_tag: String::new(),
    };
    journal.auth_tag = authenticate_pull_recovery_journal(auth_key, &journal)?;
    let bytes = lillux::canonical_json(&serde_json::to_value(&journal)?)?.into_bytes();
    if bytes.len() > MAX_PULL_RECOVERY_JOURNAL_BYTES {
        return Err(PullResultsError::Other(anyhow::anyhow!(
            "remote result recovery journal is {} bytes, exceeding the recoverable limit of {MAX_PULL_RECOVERY_JOURNAL_BYTES}",
            bytes.len()
        )));
    }
    if backup
        .atomic_create_regular(std::ffi::OsStr::new("journal.json"), &bytes, 0o600)?
        .is_none()
    {
        return Err(PullResultsError::Other(anyhow::anyhow!(
            "pull recovery journal already exists"
        )));
    }
    backup.try_clone_descriptor()?.sync_all()?;
    Ok(())
}

fn authenticate_pull_recovery_journal(
    auth_key: &[u8; 32],
    journal: &PullRecoveryJournal,
) -> Result<String, PullResultsError> {
    let authenticated = lillux::canonical_json(&journal.authenticated_value())?;
    let mut mac = <Hmac<Sha256>>::new_from_slice(auth_key)
        .map_err(|_| anyhow::anyhow!("invalid remote pull journal authentication key"))?;
    mac.update(authenticated.as_bytes());
    Ok(mac
        .finalize()
        .into_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn read_authenticated_pull_recovery_journal(
    project_root: &lillux::PinnedDirectory,
    backup_name: &std::ffi::OsStr,
    backup: &lillux::PinnedDirectory,
    auth_key: &[u8; 32],
) -> Result<PullRecoveryJournal, PullResultsError> {
    let journal_path = project_root.path().join(backup_name).join("journal.json");
    let mut journal_file = backup
        .open_regular(std::ffi::OsStr::new("journal.json"), false)?
        .ok_or_else(|| {
            PullResultsError::RecoveryRequired(format!(
                "pull recovery directory {} has no authenticated journal",
                project_root.path().join(backup_name).display()
            ))
        })?;
    let mut bytes = Vec::new();
    std::io::Read::take(
        &mut journal_file,
        u64::try_from(MAX_PULL_RECOVERY_JOURNAL_BYTES + 1).expect("journal bound fits in u64"),
    )
    .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_PULL_RECOVERY_JOURNAL_BYTES {
        return Err(PullResultsError::RecoveryRequired(format!(
            "pull recovery journal {} exceeds {} bytes",
            journal_path.display(),
            MAX_PULL_RECOVERY_JOURNAL_BYTES
        )));
    }
    let journal: PullRecoveryJournal = serde_json::from_slice(&bytes).map_err(|error| {
        PullResultsError::RecoveryRequired(format!(
            "invalid pull recovery journal {}: {error}",
            journal_path.display()
        ))
    })?;
    let canonical = lillux::canonical_json(&serde_json::to_value(&journal)?)?;
    if canonical.as_bytes() != bytes {
        return Err(PullResultsError::RecoveryRequired(format!(
            "pull recovery journal {} is not canonical JSON",
            journal_path.display()
        )));
    }
    validate_pull_recovery_journal(project_root, backup_name, &journal)?;
    let supplied_tag = decode_sha256_hex("pull recovery auth_tag", &journal.auth_tag)?;
    let authenticated = lillux::canonical_json(&journal.authenticated_value())?;
    let mut mac = <Hmac<Sha256>>::new_from_slice(auth_key)
        .map_err(|_| anyhow::anyhow!("invalid remote pull journal authentication key"))?;
    mac.update(authenticated.as_bytes());
    mac.verify_slice(&supplied_tag).map_err(|_| {
        PullResultsError::RecoveryRequired(format!(
            "pull recovery journal {} is not authenticated by this node",
            journal_path.display()
        ))
    })?;
    Ok(journal)
}

fn validate_pull_recovery_journal(
    project_root: &lillux::PinnedDirectory,
    backup_name: &std::ffi::OsStr,
    journal: &PullRecoveryJournal,
) -> Result<(), PullResultsError> {
    if journal.kind != PULL_RECOVERY_JOURNAL_KIND || journal.schema != PULL_RECOVERY_JOURNAL_SCHEMA
    {
        return Err(PullResultsError::RecoveryRequired(
            "unsupported pull recovery journal kind or schema".to_owned(),
        ));
    }
    if journal.transaction_id.len() != 32
        || !journal
            .transaction_id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(PullResultsError::RecoveryRequired(
            "pull recovery journal has a non-canonical transaction id".to_owned(),
        ));
    }
    let expected_backup = format!(".ryeos-pull-backup-{}", journal.transaction_id);
    if backup_name != std::ffi::OsStr::new(&expected_backup) {
        return Err(PullResultsError::RecoveryRequired(
            "pull recovery journal is not bound to its backup directory".to_owned(),
        ));
    }
    if project_root.path().to_str() != Some(journal.project_root.as_str()) {
        return Err(PullResultsError::RecoveryRequired(
            "pull recovery journal is bound to a different project root".to_owned(),
        ));
    }
    if journal.changes.len() > MAX_PULL_RECOVERY_CHANGES {
        return Err(PullResultsError::RecoveryRequired(format!(
            "pull recovery journal exceeds {MAX_PULL_RECOVERY_CHANGES} changes"
        )));
    }
    let mut paths = std::collections::BTreeSet::new();
    let mut artifacts = std::collections::BTreeSet::new();
    for change in &journal.changes {
        let (path, artifact, base, result) = match change {
            PullRecoveryChange::Write {
                path,
                artifact,
                base,
                result,
            } => (path, artifact, base.as_ref(), Some(result)),
            PullRecoveryChange::Delete {
                path,
                artifact,
                base,
            } => (path, artifact, Some(base), None),
        };
        ryeos_state::project_sync::validate_safe_relative_path(path).map_err(|error| {
            PullResultsError::RecoveryRequired(format!(
                "pull recovery journal path {path:?} is unsafe: {error:#}"
            ))
        })?;
        if ryeos_state::project_sync::is_project_snapshot_floor_excluded(path) {
            return Err(PullResultsError::RecoveryRequired(format!(
                "pull recovery journal targets reserved path {path:?}"
            )));
        }
        let expected_artifact = scratch_artifact_name(path).to_string_lossy().into_owned();
        if artifact != &expected_artifact {
            return Err(PullResultsError::RecoveryRequired(format!(
                "pull recovery artifact does not match path {path:?}"
            )));
        }
        if !paths.insert(path.as_str()) || !artifacts.insert(artifact.as_str()) {
            return Err(PullResultsError::RecoveryRequired(
                "pull recovery journal contains duplicate paths or artifacts".to_owned(),
            ));
        }
        if let Some(base) = base {
            validate_recovery_file_identity("base", base)?;
        }
        if let Some(result) = result {
            validate_recovery_file_identity("result", result)?;
        }
    }
    decode_sha256_hex("pull recovery auth_tag", &journal.auth_tag)?;
    Ok(())
}

fn validate_recovery_file_identity(
    label: &str,
    identity: &RecoveryFileIdentity,
) -> Result<(), PullResultsError> {
    decode_sha256_hex(label, &identity.blob_hash)?;
    if !matches!(
        identity.normalized_mode,
        ProjectFile::REGULAR_MODE | ProjectFile::EXECUTABLE_MODE
    ) {
        return Err(PullResultsError::RecoveryRequired(format!(
            "pull recovery {label} mode is not normalized"
        )));
    }
    Ok(())
}

fn decode_sha256_hex(label: &str, value: &str) -> Result<[u8; 32], PullResultsError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(PullResultsError::RecoveryRequired(format!(
            "{label} is not canonical lowercase sha256"
        )));
    }
    let mut decoded = [0_u8; 32];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        let high = (pair[0] as char).to_digit(16).expect("validated hex") as u8;
        let low = (pair[1] as char).to_digit(16).expect("validated hex") as u8;
        decoded[index] = (high << 4) | low;
    }
    Ok(decoded)
}

fn materialize_recovered_base(
    project_root: &lillux::PinnedDirectory,
    cas: &CasStore,
    transaction_id: &str,
    rel_path: &str,
    base: &RecoveryFileIdentity,
) -> Result<PathBuf, PullResultsError> {
    let (parent, _) = pinned_relative_parent(project_root, rel_path, true)?
        .ok_or_else(|| anyhow::anyhow!("recovery parent could not be opened"))?;
    parent.ensure_path_binding()?;
    let recovered_name = std::ffi::OsString::from(format!(
        ".ryeos-recovered-base-{}",
        lillux::sha256_hex(format!("{transaction_id}\0{rel_path}").as_bytes())
    ));
    if let Some(existing) = parent.open_regular(&recovered_name, false)? {
        validate_expected_content(
            &existing,
            rel_path,
            Some(base.blob_hash.as_str()),
            Some(base.normalized_mode),
        )?;
    } else {
        let recovered_path = parent.descriptor_child_path(&recovered_name)?;
        match cas.materialize_blob_to_new_file(
            &base.blob_hash,
            &recovered_path,
            base.normalized_mode,
        ) {
            Ok(_) => {}
            Err(error) => {
                let existing = parent
                    .open_regular(&recovered_name, false)?
                    .ok_or_else(|| {
                        PullResultsError::RecoveryRequired(format!(
                            "could not materialize recovered base for {rel_path}: {error:#}"
                        ))
                    })?;
                validate_expected_content(
                    &existing,
                    rel_path,
                    Some(base.blob_hash.as_str()),
                    Some(base.normalized_mode),
                )?;
            }
        }
    }
    parent.ensure_path_binding()?;
    project_root.ensure_path_binding()?;
    Ok(parent.path().join(recovered_name))
}

struct PinnedScratchDirectory {
    parent: lillux::PinnedDirectory,
    name: std::ffi::OsString,
    directory: lillux::PinnedDirectory,
    cleanup_on_drop: bool,
}

impl PinnedScratchDirectory {
    fn create(root: &lillux::PinnedDirectory, name: &str) -> Result<Self, PullResultsError> {
        let name = std::ffi::OsString::from(name);
        let directory = root.create_child(&name, 0o700)?;
        Ok(Self {
            parent: root.try_clone()?,
            name,
            directory,
            cleanup_on_drop: true,
        })
    }

    fn directory(&self) -> &lillux::PinnedDirectory {
        &self.directory
    }

    fn preserve(&mut self) {
        self.cleanup_on_drop = false;
        tracing::error!(
            path = %self.parent.path().join(&self.name).display(),
            "preserving remote result recovery artifacts"
        );
    }
}

impl Drop for PinnedScratchDirectory {
    fn drop(&mut self) {
        if !self.cleanup_on_drop {
            return;
        }
        if let Err(error) = self.directory.remove_contents_recursive().and_then(|()| {
            self.parent
                .remove_empty_child_if_same(&self.name, &self.directory)
                .and_then(|removed| {
                    if removed {
                        Ok(())
                    } else {
                        anyhow::bail!("scratch directory remained non-empty")
                    }
                })
        }) {
            tracing::warn!(%error, "failed to clean pinned pull/apply scratch directory");
        }
    }
}

fn ensure_no_pull_recovery_artifacts(
    project_root: &lillux::PinnedDirectory,
) -> Result<(), PullResultsError> {
    let found = std::cell::RefCell::new(None::<std::path::PathBuf>);
    project_root.visit_regular_files(
        |relative, _directory| {
            if is_pull_transaction_path(relative) {
                *found.borrow_mut() = Some(relative.to_path_buf());
                return Ok(true);
            }
            Ok(false)
        },
        |relative, _file| {
            if is_pull_transaction_path(relative) {
                *found.borrow_mut() = Some(relative.to_path_buf());
            }
            Ok(())
        },
    )?;
    if let Some(relative) = found.into_inner() {
        return Err(PullResultsError::RecoveryRequired(format!(
            "reserved transaction artifact {} is present; preserve it and recover the interrupted apply before retrying",
            project_root.path().join(relative).display()
        )));
    }
    Ok(())
}

fn is_pull_transaction_artifact_name(name: &str) -> bool {
    name.starts_with(".ryeos-pull-staging-")
        || name.starts_with(".ryeos-pull-backup-")
        || name.starts_with(".ryeos-quarantine.")
}

fn acquire_pull_apply_lock(
    project_root: &lillux::PinnedDirectory,
) -> Result<lillux::ExclusiveFileLock, PullResultsError> {
    project_root.ensure_path_binding()?;
    let lock = lillux::ExclusiveFileLock::acquire_in_with_timeout(
        project_root,
        std::ffi::OsStr::new("ryeos-pull"),
        std::time::Duration::from_secs(5),
    )
    .map_err(|error| {
        PullResultsError::RecoveryRequired(format!(
            "could not acquire the project pull/apply lock: {error:#}"
        ))
    })?;
    project_root.ensure_path_binding()?;
    Ok(lock)
}

fn recover_interrupted_pull_apply(
    project_root: &lillux::PinnedDirectory,
    cas: &CasStore,
    auth_key: &[u8; 32],
) -> Result<Option<String>, PullResultsError> {
    project_root.ensure_path_binding()?;
    let names = project_root.entry_names()?;
    let mut backup_names = names
        .iter()
        .filter(|name| {
            name.to_str()
                .is_some_and(|name| name.starts_with(".ryeos-pull-backup-"))
        })
        .cloned()
        .collect::<Vec<_>>();
    backup_names.sort();
    let had_artifacts = !backup_names.is_empty()
        || names.iter().any(|name| {
            name.to_str()
                .is_some_and(|name| name.starts_with(".ryeos-pull-staging-"))
        });
    let mut notices = Vec::new();
    let mut recovered_transactions = std::collections::BTreeSet::new();
    for backup_name in backup_names {
        project_root.ensure_path_binding()?;
        let backup = project_root
            .open_child_directory(&backup_name)?
            .ok_or_else(|| anyhow::anyhow!("pull recovery backup disappeared"))?;
        if backup
            .open_regular(std::ffi::OsStr::new("journal.json"), false)?
            .is_none()
        {
            let backup_name_str = backup_name.to_str().ok_or_else(|| {
                PullResultsError::RecoveryRequired(
                    "pull recovery backup name is not UTF-8".to_owned(),
                )
            })?;
            let transaction_id = backup_name_str
                .strip_prefix(".ryeos-pull-backup-")
                .filter(|value| {
                    value.len() == 32
                        && value
                            .bytes()
                            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                })
                .ok_or_else(|| {
                    PullResultsError::RecoveryRequired(format!(
                        "unauthenticated pull recovery directory {backup_name_str:?} has a non-canonical transaction id"
                    ))
                })?;
            let staging_name =
                std::ffi::OsString::from(format!(".ryeos-pull-staging-{transaction_id}"));
            if project_root.open_child_directory(&staging_name)?.is_some() {
                return Err(PullResultsError::RecoveryRequired(format!(
                    "pull transaction {transaction_id} has staging bytes but no authenticated journal; preserve it for operator inspection"
                )));
            }
            project_root.ensure_path_binding()?;
            if !project_root.remove_empty_child_if_same(&backup_name, &backup)? {
                return Err(PullResultsError::RecoveryRequired(format!(
                    "pre-journal pull directory {backup_name_str:?} was not empty; preserve it for operator inspection"
                )));
            }
            project_root.ensure_path_binding()?;
            notices.push(format!(
                "retired pre-journal pull transaction {transaction_id} without changing project bytes"
            ));
            continue;
        }
        let journal = read_authenticated_pull_recovery_journal(
            project_root,
            &backup_name,
            &backup,
            auth_key,
        )?;
        recovered_transactions.insert(journal.transaction_id.clone());
        for change in &journal.changes {
            recover_journaled_change(
                project_root,
                &backup,
                cas,
                &journal.transaction_id,
                change,
                &mut notices,
            )?;
        }
        project_root.ensure_path_binding()?;
        cleanup_recovery_directory(project_root, &backup_name, &backup)?;
        let staging_name =
            std::ffi::OsString::from(format!(".ryeos-pull-staging-{}", journal.transaction_id));
        if let Some(staging) = project_root.open_child_directory(&staging_name)? {
            cleanup_recovery_directory(project_root, &staging_name, &staging)?;
        }
    }
    for name in &names {
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(transaction_id) = name.strip_prefix(".ryeos-pull-staging-") else {
            continue;
        };
        if !recovered_transactions.contains(transaction_id)
            && project_root
                .open_child_directory(std::ffi::OsStr::new(name))?
                .is_some()
        {
            return Err(PullResultsError::RecoveryRequired(format!(
                "unpaired pull staging directory {name:?} has no authenticated recovery journal; preserve it for operator inspection"
            )));
        }
    }
    let surfaced = surface_quarantined_files(project_root)?;
    notices.extend(surfaced);
    project_root.ensure_path_binding()?;
    if had_artifacts || !notices.is_empty() {
        let detail = if notices.is_empty() {
            "interrupted remote result apply was retired without changing live project bytes"
                .to_owned()
        } else {
            notices.join("; ")
        };
        return Ok(Some(format!("{detail}; retry the pull")));
    }
    Ok(None)
}

fn recover_journaled_change(
    project_root: &lillux::PinnedDirectory,
    backup: &lillux::PinnedDirectory,
    cas: &CasStore,
    transaction_id: &str,
    change: &PullRecoveryChange,
    notices: &mut Vec<String>,
) -> Result<(), PullResultsError> {
    let (rel_path, artifact, base, result) = match change {
        PullRecoveryChange::Write {
            path,
            artifact,
            base,
            result,
        } => (
            path.as_str(),
            artifact.as_str(),
            base.as_ref(),
            Some(result),
        ),
        PullRecoveryChange::Delete {
            path,
            artifact,
            base,
        } => (path.as_str(), artifact.as_str(), Some(base), None),
    };
    project_root.ensure_path_binding()?;
    if let Some(base) = base {
        if let Some(saved) = backup.open_regular(std::ffi::OsStr::new(artifact), false)? {
            validate_expected_content(
                &saved,
                rel_path,
                Some(base.blob_hash.as_str()),
                Some(base.normalized_mode),
            )?;
        }
    } else if backup
        .open_regular(std::ffi::OsStr::new(artifact), false)?
        .is_some()
    {
        return Err(PullResultsError::RecoveryRequired(format!(
            "new-file recovery for {rel_path} carried a contradictory base backup"
        )));
    }
    let live = open_relative_regular(project_root, rel_path)?;
    let live_state = live
        .as_ref()
        .map(|file| {
            Ok::<_, PullResultsError>((
                hash_open_regular(file.try_clone().map_err(anyhow::Error::from)?)?,
                normalized_open_mode(file)?,
            ))
        })
        .transpose()?;
    let already_base = match (base, live_state.as_ref()) {
        (None, None) => true,
        (Some(base), Some((live_hash, live_mode))) => {
            base.blob_hash == *live_hash && base.normalized_mode == *live_mode
        }
        _ => false,
    };
    if already_base {
        return Ok(());
    }
    if let Some(base) = base {
        let recovered =
            materialize_recovered_base(project_root, cas, transaction_id, rel_path, base)?;
        notices.push(format!(
            "preserved the pre-pull version of {rel_path} as {}",
            recovered.display()
        ));
    }
    let live_description = match (result, live_state.as_ref()) {
        (Some(result), Some((hash, mode)))
            if result.blob_hash == *hash && result.normalized_mode == *mode =>
        {
            "the remote result"
        }
        (None, None) => "an absent path",
        _ => "concurrent local state",
    };
    notices.push(format!(
        "left {live_description} untouched at {rel_path} after interrupted pull"
    ));
    project_root.ensure_path_binding()?;
    Ok(())
}

fn cleanup_recovery_directory(
    project_root: &lillux::PinnedDirectory,
    name: &std::ffi::OsStr,
    directory: &lillux::PinnedDirectory,
) -> Result<(), PullResultsError> {
    project_root.ensure_path_binding()?;
    directory.remove_contents_recursive()?;
    if !project_root.remove_empty_child_if_same(name, directory)? {
        return Err(PullResultsError::RecoveryRequired(format!(
            "recovery directory {} remained non-empty",
            project_root.path().join(name).display()
        )));
    }
    project_root.ensure_path_binding()?;
    Ok(())
}

fn surface_quarantined_files(
    project_root: &lillux::PinnedDirectory,
) -> Result<Vec<String>, PullResultsError> {
    let quarantines = std::cell::RefCell::new(Vec::<PathBuf>::new());
    project_root.visit_regular_files(
        |_relative, _directory| Ok(false),
        |relative, _file| {
            if relative.components().any(|component| {
                matches!(component, std::path::Component::Normal(name) if name.to_str().is_some_and(|name| name.starts_with(".ryeos-quarantine.")))
            }) {
                quarantines.borrow_mut().push(relative.to_path_buf());
            }
            Ok(())
        },
    )?;
    let mut notices = Vec::new();
    for relative in quarantines.into_inner() {
        project_root.ensure_path_binding()?;
        let relative_str = relative
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("quarantine path is not UTF-8"))?;
        let (parent, name) = pinned_relative_parent(project_root, relative_str, false)?
            .ok_or_else(|| anyhow::anyhow!("quarantine parent disappeared"))?;
        let file = parent
            .open_regular(&name, false)?
            .ok_or_else(|| anyhow::anyhow!("quarantine file disappeared"))?;
        let recovered_name = std::ffi::OsString::from(format!(
            ".ryeos-recovered-{}",
            lillux::sha256_hex(relative_str.as_bytes())
        ));
        parent.ensure_path_binding()?;
        if let Err(error) =
            parent.rename_regular_child_noreplace_atomic(&name, &recovered_name, &file)
        {
            if !(error.namespace_committed()
                && parent
                    .ensure_regular_entry_matches(&recovered_name, Some(&file))
                    .is_ok())
            {
                return Err(PullResultsError::Other(error.into()));
            }
            tracing::warn!(
                path = %parent.path().join(&recovered_name).display(),
                %error,
                "quarantine preservation committed but directory durability was uncertain"
            );
        }
        parent.ensure_path_binding()?;
        project_root.ensure_path_binding()?;
        notices.push(format!(
            "preserved ambiguous local bytes as {}",
            parent.path().join(recovered_name).display()
        ));
    }
    project_root.ensure_path_binding()?;
    Ok(notices)
}

fn is_pull_transaction_path(path: &Path) -> bool {
    path.components().any(|component| {
        let std::path::Component::Normal(name) = component else {
            return false;
        };
        name.to_str().is_some_and(is_pull_transaction_artifact_name)
    })
}

/// Roll back applied writes and deletes by restoring from backup.
///
/// For files that had a backup (existed before apply), the backup is
/// restored over the live path. For files that had **no** backup (new
/// files created during apply), the live path is **removed** entirely.
#[cfg(test)]
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

#[cfg(test)]
fn hash_local_regular(path: &Path) -> Result<Option<String>, PullResultsError> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => metadata,
        Ok(_) => return Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(PullResultsError::Other(error.into())),
    };
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .ok_or_else(|| PullResultsError::Other(anyhow::anyhow!("local file has no name")))?;
    let pinned = lillux::PinnedDirectory::open(parent)?
        .ok_or_else(|| anyhow::anyhow!("local file parent disappeared"))?;
    let mut source = match pinned.open_regular(name, false)? {
        Some(source) => source,
        None => return Ok(None),
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let opened = source.metadata().map_err(anyhow::Error::from)?;
        if opened.dev() != metadata.dev() || opened.ino() != metadata.ino() {
            return Err(PullResultsError::LocalConflict(path.display().to_string()));
        }
    }
    use sha2::Digest as _;
    let mut digest = sha2::Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let read = source.read(&mut buffer).map_err(anyhow::Error::from)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let after = source.metadata().map_err(anyhow::Error::from)?;
        if after.dev() != metadata.dev()
            || after.ino() != metadata.ino()
            || after.size() != metadata.size()
            || after.mtime() != metadata.mtime()
            || after.mtime_nsec() != metadata.mtime_nsec()
            || after.ctime() != metadata.ctime()
            || after.ctime_nsec() != metadata.ctime_nsec()
        {
            return Err(PullResultsError::LocalConflict(path.display().to_string()));
        }
    }
    Ok(Some(format!("{:x}", digest.finalize())))
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
        .map(|arr| arr.iter().any(|p| p.as_str() == Some(pushed_snapshot_hash)))
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

/// Safely join a base directory with a relative path from a remote project tree.
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
#[cfg(test)]
fn safe_join(base: &Path, rel: &str) -> Result<PathBuf, PullResultsError> {
    if rel.is_empty() {
        return Ok(base.to_path_buf());
    }
    // Reject absolute paths
    if rel.starts_with('/') {
        return Err(PullResultsError::Other(anyhow::anyhow!(
            "rejecting absolute path '{}' from remote project tree",
            rel
        )));
    }
    // Reject NUL bytes
    if rel.contains('\0') {
        return Err(PullResultsError::Other(anyhow::anyhow!(
            "rejecting path with NUL byte from remote project tree: '{}'",
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
                        "rejecting path '{}' from remote project tree: escapes base directory (.. traversal)",
                        rel
                    )));
                }
            }
            std::path::Component::Normal(seg) => {
                normalized.push(seg);
            }
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                return Err(PullResultsError::Other(anyhow::anyhow!(
                    "rejecting path '{}' from remote project tree: unexpected component type",
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
                        "rejecting path '{}' from remote project tree: \
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

/// Fetch one project file's exact content and normalized mode from local CAS.
///
/// Returns an error (not `None`) if the content is missing from CAS.
/// This prevents silent partial applies.
fn require_project_file_blob(
    cas: &CasStore,
    file_hash: &str,
    rel_path: &str,
) -> Result<ProjectFile, PullResultsError> {
    let file = load_project_file(cas, file_hash)?;
    let Some((_source, actual_size)) = cas.open_blob(&file.blob_hash)? else {
        return Err(PullResultsError::InvalidRemoteSnapshot(format!(
            "project file {rel_path:?} blob {} is missing",
            file.blob_hash
        )));
    };
    if actual_size != file.size {
        return Err(PullResultsError::InvalidRemoteSnapshot(format!(
            "project file {rel_path:?} declares {} bytes but its blob has {}",
            file.size, actual_size
        )));
    }
    Ok(file)
}

fn load_project_file(cas: &CasStore, file_hash: &str) -> Result<ProjectFile, PullResultsError> {
    let limits = ryeos_state::object_closure::ObjectClosureLimits::for_project_snapshot_transport();
    let object = ryeos_state::object_closure::load_exact_cas_object_with_cas(
        cas,
        file_hash,
        limits.max_object_bytes,
    )
    .map_err(PullResultsError::Other)?;
    ProjectFile::from_value(&object).map_err(PullResultsError::Other)
}

#[cfg(test)]
fn local_normalized_mode(path: &Path) -> Option<u32> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if !metadata.file_type().is_file() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        Some(ProjectFile::normalize_mode(metadata.permissions().mode()))
    }
    #[cfg(not(unix))]
    {
        Some(ProjectFile::REGULAR_MODE)
    }
}

/// Extract snapshot_hash from a remote execute response.
///
/// The snapshot hash is set by fold-back as a thread facet.
pub fn extract_snapshot_hash(response: &Value) -> Option<String> {
    // Try from the top-level thread result
    response
        .get("snapshot_hash")
        .and_then(|v| v.as_str())
        .map(String::from)
        .or_else(|| {
            // Try from facets
            response
                .get("facets")
                .and_then(|f| f.get("snapshot_hash"))
                .and_then(|v| v.as_str())
                .map(String::from)
        })
        .or_else(|| {
            // Try from nested result structure
            response
                .get("result")
                .and_then(|r| r.get("snapshot_hash"))
                .and_then(|v| v.as_str())
                .map(String::from)
        })
        .or_else(|| {
            // Try from thread.result (dispatch wraps result as {thread:{...}, result:{...}})
            response
                .get("thread")
                .and_then(|t| t.get("result"))
                .and_then(|r| r.get("snapshot_hash"))
                .and_then(|v| v.as_str())
                .map(String::from)
        })
        .or_else(|| {
            // Try from thread facets
            response
                .get("thread")
                .and_then(|t| t.get("facets"))
                .and_then(|f| f.get("snapshot_hash"))
                .and_then(|v| v.as_str())
                .map(String::from)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use lillux::cas::CasStore;
    use std::path::PathBuf;

    /// Store one current project-file object and its content blob.
    fn store_project_file(cas: &CasStore, content: &[u8]) -> String {
        let blob_hash = cas.store_blob(content).unwrap();
        cas.store_object(
            &ProjectFile {
                blob_hash,
                size: content.len() as u64,
                normalized_mode: 0o644,
            }
            .to_value(),
        )
        .unwrap()
    }

    fn make_tree(items: &[(&str, &str)]) -> ProjectTree {
        ProjectTree {
            files: items
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    // ── R3-3a: pull/apply conflict detection ──

    #[test]
    fn repeated_descriptor_hashes_restart_at_the_beginning() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("value"), b"non-empty").unwrap();
        let root = lillux::PinnedDirectory::open(tmp.path()).unwrap().unwrap();
        let file = root
            .open_regular(std::ffi::OsStr::new("value"), false)
            .unwrap()
            .unwrap();
        let expected = lillux::sha256_hex(b"non-empty");
        assert_eq!(
            hash_open_regular(file.try_clone().unwrap()).unwrap(),
            expected
        );
        assert_eq!(
            hash_open_regular(file.try_clone().unwrap()).unwrap(),
            expected
        );
    }

    #[test]
    fn apply_detects_local_conflict_on_tracked_file() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path();

        // Write a file with hash "aaa..." but its ProjectFile says it should be "bbb..."
        std::fs::write(project_root.join("file.txt"), "original content").unwrap();

        let cas_root = tmp.path().join("cas");
        let cas = CasStore::new(cas_root);
        let base_file = store_project_file(&cas, b"different base content");
        let remote_file = store_project_file(&cas, b"remote content");
        let base = make_tree(&[("file.txt", &base_file)]);
        let remote = make_tree(&[("file.txt", &remote_file)]);

        let result = apply_tree_diff(&cas, project_root, &base, &remote, &[7_u8; 32]);
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

        let cas_root = tmp.path().join("cas");
        let cas = CasStore::new(cas_root);
        let remote_file = store_project_file(&cas, b"remote");
        let base = make_tree(&[]); // empty base — file wasn't tracked
        let remote = make_tree(&[("new.txt", &remote_file)]);

        let result = apply_tree_diff(&cas, project_root, &base, &remote, &[7_u8; 32]);
        match result {
            Err(PullResultsError::LocalConflict(msg)) => {
                assert!(msg.contains("new.txt"), "got: {msg}");
                assert!(
                    msg.contains("new remote file would overwrite local"),
                    "got: {msg}"
                );
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
        let hash_a = store_project_file(&cas, b"new A content");
        let hash_a_old = store_project_file(&cas, content_a.as_bytes());
        let hash_b_old = store_project_file(&cas, content_b.as_bytes());
        let missing = cas
            .store_object(
                &ProjectFile {
                    blob_hash: "ab".repeat(32),
                    size: 8,
                    normalized_mode: 0o644,
                }
                .to_value(),
            )
            .unwrap();
        // File B's hash doesn't exist in CAS — will trigger strict failure

        let base = make_tree(&[("a.txt", &hash_a_old), ("b.txt", &hash_b_old)]);
        let remote = make_tree(&[("a.txt", &hash_a), ("b.txt", &missing)]);

        let result = apply_tree_diff(&cas, project_root, &base, &remote, &[7_u8; 32]);
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

        let hash_a_old = store_project_file(&cas, content_a.as_bytes());
        let hash_b_old = store_project_file(&cas, content_b.as_bytes());
        let hash_c = store_project_file(&cas, content_c.as_bytes());

        let hash_a_new = store_project_file(&cas, b"new A");
        let hash_d = store_project_file(&cas, b"brand new D");

        let base = make_tree(&[
            ("a.txt", &hash_a_old),
            ("b.txt", &hash_b_old),
            ("c.txt", &hash_c),
        ]);
        let remote = make_tree(&[
            ("a.txt", &hash_a_new),
            // b.txt absent → deleted
            ("c.txt", &hash_c),
            ("d.txt", &hash_d),
        ]);

        let (updated, deleted) =
            apply_tree_diff(&cas, project_root, &base, &remote, &[7_u8; 32]).unwrap();
        assert_eq!(updated, 2); // a.txt + d.txt
        assert_eq!(deleted, 1); // b.txt

        assert_eq!(
            std::fs::read_to_string(project_root.join("a.txt")).unwrap(),
            "new A"
        );
        assert!(
            !project_root.join("b.txt").exists(),
            "b.txt should be deleted"
        );
        assert_eq!(
            std::fs::read_to_string(project_root.join("c.txt")).unwrap(),
            "unchanged"
        );
        assert_eq!(
            std::fs::read_to_string(project_root.join("d.txt")).unwrap(),
            "brand new D"
        );
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

        rollback_writes_and_deletes(&[], &["b.txt".into()], &backup_dir, project_root);

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
            "project_tree_hash": "tree_x",
            "parent_hashes": [],
        });
        verify_snapshot_lineage(&snap, "abc", "abc").expect("identical hash must accept");
    }

    #[test]
    fn lineage_accepts_direct_parent() {
        // result.parent_hashes contains pushed hash
        let snap = serde_json::json!({
            "project_tree_hash": "tree_y",
            "parent_hashes": ["pushed_xxx", "other_yyy"],
        });
        verify_snapshot_lineage(&snap, "pushed_xxx", "result_zzz")
            .expect("direct parent must accept");
    }

    #[test]
    fn lineage_rejects_unrelated_snapshot() {
        // Different hash, pushed NOT in parents
        let snap = serde_json::json!({
            "project_tree_hash": "tree_z",
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
            "project_tree_hash": "tree_z",
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

        let err =
            safe_join(project, "src/file.txt").expect_err("intermediate symlink must be rejected");
        let msg = format!("{:?}", err);
        assert!(
            msg.contains("symlink"),
            "expected symlink rejection, got: {msg}"
        );
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
            "project_tree_hash": "tree_z",
            "parent_hashes": ["intermediate_iii"],
        });
        // pushed_xxx is NOT in parent_hashes (would be parent's parent in
        // a real chain) — we have no way to verify, so reject.
        let err = verify_snapshot_lineage(&snap, "pushed_xxx", "result_zzz")
            .expect_err("non-direct-parent must reject");
        assert!(matches!(err, PullResultsError::UnrelatedSnapshot { .. }));
    }
}
