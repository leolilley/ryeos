//! CAS push pipeline for remote nodes.
//!
//! Handles the ingest-locally → upload-blobs → push-head pipeline
//! for pushing project content to a remote node.

use std::io::Read as _;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use base64::Engine as _;

use lillux::cas::CasStore;
use ryeos_state::ignore::IgnoreMatcher;
use ryeos_state::objects::{ProjectSnapshot, ProjectTree};
use ryeos_state::project_sync::ProjectSyncScope;
use ryeos_state::{PinnedStateAuthority, StagedCasRootLease};

use crate::remote::client::{BlobChunkUpload, BlobUpload, ObjectsPutResponse, RemoteClient};
use ryeos_app::state::AppState;

const OBJECTS_PUT_BODY_BUDGET_BYTES: usize = 900 * 1024;

/// Result of pushing a project to a remote.
#[derive(Debug)]
pub struct PushResult {
    pub snapshot_hash: String,
    pub tree_hash: String,
    /// The exact pushed tree — needed by pull_results() for
    /// conflict detection (can't recompute later; workspace may drift).
    pub tree: ProjectTree,
    pub tree_entries: usize,
    pub blobs_uploaded: usize,
    pub blobs_skipped: usize,
}

/// Upload one already-admitted project generation without reopening or
/// recapturing a live project. The destination HEAD boundary must be an exact
/// parent of the selected snapshot; otherwise forwarding fails rather than
/// manufacturing a replacement snapshot with different authority.
pub async fn push_snapshot_generation(
    client: &RemoteClient,
    authority: &PinnedStateAuthority,
    snapshot_hash: &str,
    project_path_for_ref: &str,
) -> Result<PushResult> {
    let local_cas = authority.cas_store()?;
    let snapshot_value = local_cas
        .get_object(snapshot_hash)?
        .ok_or_else(|| anyhow::anyhow!("project snapshot {snapshot_hash} is absent"))?;
    let snapshot = ProjectSnapshot::from_value(&snapshot_value)?;
    let tree_value = local_cas
        .get_object(&snapshot.project_tree_hash)?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "project tree {} for snapshot {snapshot_hash} is absent",
                snapshot.project_tree_hash
            )
        })?;
    let tree = ProjectTree::from_value(&tree_value)?;
    let recovery = authority.require_recovery()?;
    let mut staged_roots = {
        let guard = authority.acquire_shared_guard()?;
        recovery.begin_staged_cas_roots_admitted(&guard, "remote-push-pinned-generation")?
    };

    let operation = async {
        // A lost acknowledgement may leave the exact generation already
        // published. Check the owner-bound HEAD before opening a staging
        // session so idempotent recovery cannot leak upload reservations.
        let status = client.project_status_bounded(project_path_for_ref).await?;
        if status.get("deployed").and_then(serde_json::Value::as_bool) == Some(true)
            && status
                .get("deployed_snapshot_hash")
                .and_then(serde_json::Value::as_str)
                == Some(snapshot_hash)
        {
            return Ok(PushResult {
                snapshot_hash: snapshot_hash.to_string(),
                tree_hash: snapshot.project_tree_hash,
                tree_entries: tree.files.len(),
                tree,
                blobs_uploaded: 0,
                blobs_skipped: 0,
            });
        }
        let upload_session = client
            .objects_put(None, project_path_for_ref, &[], &[])
            .await?;
        let upload_closure = collect_snapshot_upload_hashes(
            &local_cas,
            snapshot_hash,
            upload_session.expected_previous_hash.as_deref(),
        )?;
        {
            let guard = authority.acquire_shared_guard()?;
            staged_roots.protect_cas_closure_admitted(
                &guard,
                upload_closure.object_hashes.iter().map(String::as_str),
                upload_closure.blob_hashes.iter().map(String::as_str),
            )?;
        }
        let upload = upload_missing(
            client,
            &local_cas,
            &upload_closure,
            snapshot_hash,
            project_path_for_ref,
            upload_session,
        )
        .await?;
        client
            .push_head(
                project_path_for_ref,
                snapshot_hash,
                &upload.staging_id,
                upload.expected_previous_hash.as_deref(),
            )
            .await?;

        Ok(PushResult {
            snapshot_hash: snapshot_hash.to_string(),
            tree_hash: snapshot.project_tree_hash,
            tree_entries: tree.files.len(),
            tree,
            blobs_uploaded: upload.uploaded,
            blobs_skipped: upload.skipped,
        })
    }
    .await;

    finish_staged_roots(authority, &mut staged_roots, operation)
}

/// Upload and publish one already-admitted descendant generation through an
/// upload session that was opened by the caller.
///
/// Unlike [`push_snapshot_generation`], this accepts a project DAG merge with
/// more than one direct parent. The destination-issued previous HEAD must
/// still occur in the candidate's verified history. The complete candidate
/// closure is collected locally, so every additional parent and its reachable
/// content is transferred rather than being treated as destination state.
pub async fn push_descendant_snapshot_with_session(
    client: &RemoteClient,
    authority: &PinnedStateAuthority,
    snapshot_hash: &str,
    project_path_for_ref: &str,
    upload_session: ObjectsPutResponse,
) -> Result<PushResult> {
    let expected_previous_hash = upload_session
        .expected_previous_hash
        .as_deref()
        .ok_or_else(|| {
            anyhow::anyhow!("descendant publication requires an existing remote HEAD")
        })?;
    let local_cas = authority.cas_store()?;
    if !crate::handlers::project_apply_snapshot::snapshot_history_contains(
        &local_cas,
        snapshot_hash,
        expected_previous_hash,
    )? {
        anyhow::bail!(
            "snapshot {snapshot_hash} does not descend from the server-issued previous HEAD {expected_previous_hash}"
        );
    }
    let snapshot_value = local_cas
        .get_object(snapshot_hash)?
        .ok_or_else(|| anyhow::anyhow!("project snapshot {snapshot_hash} is absent"))?;
    let snapshot = ProjectSnapshot::from_value(&snapshot_value)?;
    let tree_value = local_cas
        .get_object(&snapshot.project_tree_hash)?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "project tree {} for snapshot {snapshot_hash} is absent",
                snapshot.project_tree_hash
            )
        })?;
    let tree = ProjectTree::from_value(&tree_value)?;
    let limits = ryeos_state::object_closure::ObjectClosureLimits::for_project_snapshot_transport();
    let report = ryeos_state::object_closure::collect_object_closure_with_cas_and_limits(
        &local_cas,
        [snapshot_hash.to_owned()],
        limits,
    )?;
    if !report.is_complete() || !report.large_object_hashes.is_empty() {
        anyhow::bail!("descendant project snapshot upload closure is incomplete: {report:?}");
    }
    let upload_closure = SnapshotCasClosure {
        object_hashes: report.object_hashes.into_iter().collect(),
        blob_hashes: report.blob_hashes.into_iter().collect(),
    };
    let recovery = authority.require_recovery()?;
    let mut staged_roots = {
        let guard = authority.acquire_shared_guard()?;
        let mut roots =
            recovery.begin_staged_cas_roots_admitted(&guard, "remote-push-descendant")?;
        roots.protect_cas_closure_admitted(
            &guard,
            upload_closure.object_hashes.iter().map(String::as_str),
            upload_closure.blob_hashes.iter().map(String::as_str),
        )?;
        roots
    };

    let operation = async {
        let upload = upload_missing(
            client,
            &local_cas,
            &upload_closure,
            snapshot_hash,
            project_path_for_ref,
            upload_session,
        )
        .await?;
        client
            .push_head(
                project_path_for_ref,
                snapshot_hash,
                &upload.staging_id,
                upload.expected_previous_hash.as_deref(),
            )
            .await?;
        Ok(PushResult {
            snapshot_hash: snapshot_hash.to_owned(),
            tree_hash: snapshot.project_tree_hash,
            tree_entries: tree.files.len(),
            tree,
            blobs_uploaded: upload.uploaded,
            blobs_skipped: upload.skipped,
        })
    }
    .await;

    finish_staged_roots(authority, &mut staged_roots, operation)
}

/// Build, upload, and stage an AI-only project snapshot.
///
/// Unlike [`push_project`], this captures the curated `.ai` allow-list rather
/// than a full-project tree. It still emits the current typed project snapshot
/// closure; excluded ordinary project paths are absent from that tree.
///
/// The union of the source node's current policy and `remote_ignore` is
/// applied to every candidate path
/// under the allow-list so files the remote would later reject (e.g.
/// `__pycache__/`, `*.pyc`) are dropped client-side instead of
/// blowing up at `/push-head`.
pub async fn push_project_ai_only(
    client: &RemoteClient,
    state: &Arc<AppState>,
    authority: &PinnedStateAuthority,
    local_project_path: &Path,
    remote_project_path_for_ref: &str,
    remote_ignore: &IgnoreMatcher,
) -> Result<PushResult> {
    let app_root = &state.config.app_root;
    refuse_walking_root(local_project_path, app_root)?;

    let transfer_ignore = state.ignore_matcher.union(remote_ignore)?;
    let local_cas = authority.cas_store()?;
    let recovery = authority.require_recovery()?;
    let project_root = lillux::PinnedDirectory::open(local_project_path)?.ok_or_else(|| {
        anyhow::anyhow!(
            "project root does not exist: {}",
            local_project_path.display()
        )
    })?;
    let mut staged_roots = {
        let guard = authority.acquire_shared_guard()?;
        recovery.begin_staged_cas_roots_admitted(&guard, "remote-push-ai")?
    };

    let operation = async {
        let policy = ryeos_state::project_sync::capture_snapshot_policy_from_pinned(
            &project_root,
            &transfer_ignore,
            ProjectSyncScope::AiOnly,
        )?;
        let tree = {
            let guard = authority.acquire_shared_guard()?;
            ryeos_executor::execution::ingest::ingest_project_tree(
                authority,
                &guard,
                &project_root,
                &policy,
            )?
        };
        ryeos_state::project_sync::validate_captured_policy_source(&local_cas, &tree, &policy)?;
        if ryeos_state::project_sync::capture_snapshot_policy_from_pinned(
            &project_root,
            &transfer_ignore,
            ProjectSyncScope::AiOnly,
        )? != policy
        {
            anyhow::bail!("project snapshot policy changed during remote capture");
        }
        project_root.ensure_path_binding()?;

        let upload_session = client
            .objects_put(None, remote_project_path_for_ref, &[], &[])
            .await?;

        let tree_hash = {
            let guard = authority.acquire_shared_guard()?;
            staged_roots.store_object_admitted(&guard, &local_cas, &tree.to_value())?
        };
        let snapshot = ryeos_state::objects::ProjectSnapshot {
            project_tree_hash: tree_hash.clone(),
            effective_policy_hash: {
                let guard = authority.acquire_shared_guard()?;
                staged_roots.store_object_admitted(&guard, &local_cas, &policy.to_value())?
            },
            message: None,
            parent_hashes: upload_session
                .expected_previous_hash
                .iter()
                .cloned()
                .collect(),
            created_at: lillux::time::iso8601_now(),
            source: "remote_ai_sync".to_string(),
        };
        let (snapshot_hash, upload_closure) = {
            let guard = authority.acquire_shared_guard()?;
            let snapshot_hash =
                staged_roots.store_object_admitted(&guard, &local_cas, &snapshot.to_value())?;
            let upload_closure = collect_snapshot_upload_hashes(
                &local_cas,
                &snapshot_hash,
                upload_session.expected_previous_hash.as_deref(),
            )?;
            staged_roots.protect_cas_closure_admitted(
                &guard,
                upload_closure.object_hashes.iter().map(String::as_str),
                upload_closure.blob_hashes.iter().map(String::as_str),
            )?;
            (snapshot_hash, upload_closure)
        };

        let upload = upload_missing(
            client,
            &local_cas,
            &upload_closure,
            &snapshot_hash,
            remote_project_path_for_ref,
            upload_session,
        )
        .await?;

        client
            .push_head(
                remote_project_path_for_ref,
                &snapshot_hash,
                &upload.staging_id,
                upload.expected_previous_hash.as_deref(),
            )
            .await?;

        let tree_entries = tree.files.len();
        Ok(PushResult {
            snapshot_hash,
            tree_hash,
            tree,
            tree_entries,
            blobs_uploaded: upload.uploaded,
            blobs_skipped: upload.skipped,
        })
    }
    .await;

    finish_staged_roots(authority, &mut staged_roots, operation)
}

/// Push a project directory to a remote node.
///
/// 1. Apply the union of source and remote ingest-ignore rules
/// 2. Ingest locally into CAS
/// 3. Build manifest + snapshot
/// 4. Check which typed objects and blobs the remote already has
/// 5. Upload missing blobs and objects, including manifest + snapshot
/// 6. Call push-head to write the HEAD ref
///
/// Both policies are independently authoritative. Their exclusion union keeps
/// source-forbidden content out of local CAS/upload and target-forbidden
/// content out of the transferred generation. The target still rechecks its
/// current policy at admission. Callers must resolve the target rules before
/// calling this function.
pub async fn push_project(
    client: &RemoteClient,
    state: &Arc<AppState>,
    authority: &PinnedStateAuthority,
    project_path: &Path,
    project_path_for_ref: &str,
    remote_ignore: &IgnoreMatcher,
) -> Result<PushResult> {
    let app_root = &state.config.app_root;

    // Fail-fast guards: prevent the common footgun of running
    // `ryeos remote execute` from $HOME or some other catch-all
    // directory and silently ingest-walking thousands of unrelated
    // files. The push step recursively walks `project_path`; if
    // that's `$HOME` or contains the daemon's app root, the
    // walk takes minutes-to-hours and never produces a meaningful
    // snapshot. Detect both cases up front.
    refuse_walking_root(project_path, app_root)?;

    // 1. Ingest project directory into local CAS using remote's ignore rules.
    let transfer_ignore = state.ignore_matcher.union(remote_ignore)?;
    let local_cas = authority.cas_store()?;
    let recovery = authority.require_recovery()?;
    let project_root = lillux::PinnedDirectory::open(project_path)?.ok_or_else(|| {
        anyhow::anyhow!("project root does not exist: {}", project_path.display())
    })?;
    let mut staged_roots = {
        let guard = authority.acquire_shared_guard()?;
        recovery.begin_staged_cas_roots_admitted(&guard, "remote-push-project")?
    };

    let operation = async {
        let policy = ryeos_state::project_sync::capture_snapshot_policy_from_pinned(
            &project_root,
            &transfer_ignore,
            ProjectSyncScope::FullProject,
        )?;
        let tree = {
            let guard = authority.acquire_shared_guard()?;
            ryeos_executor::execution::ingest::ingest_project_tree(
                authority,
                &guard,
                &project_root,
                &policy,
            )?
        };
        ryeos_state::project_sync::validate_captured_policy_source(&local_cas, &tree, &policy)?;
        if ryeos_state::project_sync::capture_snapshot_policy_from_pinned(
            &project_root,
            &transfer_ignore,
            ProjectSyncScope::FullProject,
        )? != policy
        {
            anyhow::bail!("project snapshot policy changed during remote capture");
        }
        project_root.ensure_path_binding()?;

        let upload_session = client
            .objects_put(None, project_path_for_ref, &[], &[])
            .await?;

        let tree_hash = {
            let guard = authority.acquire_shared_guard()?;
            staged_roots.store_object_admitted(&guard, &local_cas, &tree.to_value())?
        };
        let snapshot = ryeos_state::objects::ProjectSnapshot {
            project_tree_hash: tree_hash.clone(),
            effective_policy_hash: {
                let guard = authority.acquire_shared_guard()?;
                staged_roots.store_object_admitted(&guard, &local_cas, &policy.to_value())?
            },
            message: None,
            parent_hashes: upload_session
                .expected_previous_hash
                .iter()
                .cloned()
                .collect(),
            created_at: lillux::time::iso8601_now(),
            source: "push".to_string(),
        };
        let (snapshot_hash, upload_closure) = {
            let guard = authority.acquire_shared_guard()?;
            let snapshot_hash =
                staged_roots.store_object_admitted(&guard, &local_cas, &snapshot.to_value())?;
            let upload_closure = collect_snapshot_upload_hashes(
                &local_cas,
                &snapshot_hash,
                upload_session.expected_previous_hash.as_deref(),
            )?;
            staged_roots.protect_cas_closure_admitted(
                &guard,
                upload_closure.object_hashes.iter().map(String::as_str),
                upload_closure.blob_hashes.iter().map(String::as_str),
            )?;
            (snapshot_hash, upload_closure)
        };
        let upload = upload_missing(
            client,
            &local_cas,
            &upload_closure,
            &snapshot_hash,
            project_path_for_ref,
            upload_session,
        )
        .await?;

        client
            .push_head(
                project_path_for_ref,
                &snapshot_hash,
                &upload.staging_id,
                upload.expected_previous_hash.as_deref(),
            )
            .await?;

        let tree_entries = tree.files.len();
        Ok(PushResult {
            snapshot_hash,
            tree_hash,
            tree,
            tree_entries,
            blobs_uploaded: upload.uploaded,
            blobs_skipped: upload.skipped,
        })
    }
    .await;

    finish_staged_roots(authority, &mut staged_roots, operation)
}

pub(crate) fn finish_staged_roots<T>(
    authority: &PinnedStateAuthority,
    staged_roots: &mut StagedCasRootLease,
    operation: Result<T>,
) -> Result<T> {
    let guard = authority.acquire_shared_guard()?;
    let finish = staged_roots.finish_admitted(&guard);
    let outcomes = (operation, finish);
    match outcomes {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error),
    }
}

#[derive(Debug)]
pub(crate) struct SnapshotCasClosure {
    pub object_hashes: Vec<String>,
    pub blob_hashes: Vec<String>,
}

pub(crate) fn collect_snapshot_upload_hashes(
    cas: &CasStore,
    snapshot_hash: &str,
    remote_known_parent: Option<&str>,
) -> Result<SnapshotCasClosure> {
    let snapshot_value = cas
        .get_object(snapshot_hash)?
        .ok_or_else(|| anyhow::anyhow!("project snapshot {snapshot_hash} is absent"))?;
    let snapshot = ProjectSnapshot::from_value(&snapshot_value)?;
    let expected_parents = remote_known_parent
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    // Re-publishing the exact current remote HEAD is idempotent. Its parent is
    // necessarily the boundary that preceded it, not the snapshot itself.
    if remote_known_parent != Some(snapshot_hash) && snapshot.parent_hashes != expected_parents {
        anyhow::bail!(
            "snapshot parent lineage does not match the server-issued previous HEAD boundary"
        );
    }
    let limits = ryeos_state::object_closure::ObjectClosureLimits::for_project_snapshot_transport();
    let mut report = ryeos_state::object_closure::collect_object_closure_with_cas_and_limits(
        cas,
        [snapshot.project_tree_hash, snapshot.effective_policy_hash],
        limits,
    )?;
    if !report.is_complete() {
        anyhow::bail!("project snapshot upload closure is incomplete: {report:?}");
    }
    report.object_hashes.insert(snapshot_hash.to_owned());
    Ok(SnapshotCasClosure {
        object_hashes: report.object_hashes.into_iter().collect(),
        blob_hashes: report.blob_hashes.into_iter().collect(),
    })
}

pub(crate) struct StagedUploadResult {
    pub staging_id: String,
    pub expected_previous_hash: Option<String>,
    pub uploaded: usize,
    pub skipped: usize,
}

pub(crate) async fn upload_missing(
    client: &RemoteClient,
    local_cas: &CasStore,
    upload_closure: &SnapshotCasClosure,
    publication_root_hash: &str,
    project_path_for_ref: &str,
    upload_session: ObjectsPutResponse,
) -> Result<StagedUploadResult> {
    if !upload_closure
        .object_hashes
        .iter()
        .any(|hash| hash == publication_root_hash)
    {
        anyhow::bail!(
            "publication root {publication_root_hash} is absent from the typed object closure"
        );
    }
    let has_resp = client
        .objects_has(&upload_closure.object_hashes, &upload_closure.blob_hashes)
        .await?;
    let skipped = has_resp.found_object_hashes.len() + has_resp.found_blob_hashes.len();
    let missing_objects = has_resp.missing_object_hashes;
    let missing_blobs = has_resp.missing_blob_hashes;
    let uploaded = missing_objects.len() + missing_blobs.len();

    let mut objects = Vec::new();
    let mut publication_root_staged = false;
    let limits = ryeos_state::object_closure::ObjectClosureLimits::for_project_snapshot_transport();
    for hash in &missing_objects {
        let value = ryeos_state::object_closure::load_exact_cas_object_with_cas(
            local_cas,
            hash,
            limits.max_object_bytes,
        )?;
        publication_root_staged |= hash == publication_root_hash;
        objects.push((hash.clone(), value));
    }

    // The final publication root must itself be bound to the server-issued
    // capability even when the remote already had a deduplicated copy.
    if !publication_root_staged {
        let root = ryeos_state::object_closure::load_exact_cas_object_with_cas(
            local_cas,
            publication_root_hash,
            limits.max_object_bytes,
        )?;
        objects.push((publication_root_hash.to_string(), root));
    }
    objects.sort_by(|left, right| left.0.cmp(&right.0));
    objects.dedup_by(|left, right| left.0 == right.0);

    let staging_id = upload_session.staging_id;
    let expected_previous_hash = upload_session.expected_previous_hash;
    validate_upload_response(&upload_session.blob_hashes, &[], "initial blob")?;
    validate_upload_response(&upload_session.object_hashes, &[], "initial object")?;

    // Pack adjacent small immutable blobs into bounded requests. Sending every
    // source file through an individual signed request makes ordinary project
    // pushes scale with network round trips rather than bytes. Large blobs
    // remain bounded, sequential and retry-idempotent chunks. Peak memory is
    // independent of generation size, and the remote still verifies every
    // complete digest before admitting it to this publication capability.
    const INLINE_BLOB_BATCH_MAX_ENTRIES: usize = 128;

    let mut blob_index = 0;
    while blob_index < missing_blobs.len() {
        let hash = &missing_blobs[blob_index];
        let (mut source, total_size) = local_cas
            .open_blob(hash)?
            .ok_or_else(|| anyhow::anyhow!("local CAS blob {hash} disappeared before upload"))?;
        if total_size > limits.max_blob_bytes {
            anyhow::bail!(
                "project blob {hash} exceeds transport limit: {total_size} > {}",
                limits.max_blob_bytes
            );
        }

        if inline_blob_request_size(total_size)? <= OBJECTS_PUT_BODY_BUDGET_BYTES {
            let mut batch = Vec::new();
            let mut expected = Vec::new();
            let mut request_size = 256_usize;
            while blob_index < missing_blobs.len() {
                if batch.len() == INLINE_BLOB_BATCH_MAX_ENTRIES {
                    break;
                }
                let candidate_hash = &missing_blobs[blob_index];
                let (mut candidate, candidate_size) =
                    local_cas.open_blob(candidate_hash)?.ok_or_else(|| {
                        anyhow::anyhow!("local CAS blob {candidate_hash} disappeared before upload")
                    })?;
                if candidate_size > limits.max_blob_bytes {
                    anyhow::bail!(
                        "project blob {candidate_hash} exceeds transport limit: {candidate_size} > {}",
                        limits.max_blob_bytes
                    );
                }
                let encoded_size = inline_blob_request_size(candidate_size)?;
                if encoded_size > OBJECTS_PUT_BODY_BUDGET_BYTES
                    || (!batch.is_empty()
                        && request_size.saturating_add(encoded_size)
                            > OBJECTS_PUT_BODY_BUDGET_BYTES)
                {
                    break;
                }
                let candidate_size = usize::try_from(candidate_size)?;
                let mut bytes = vec![0_u8; candidate_size];
                candidate.read_exact(&mut bytes)?;
                let mut trailing = [0_u8; 1];
                if candidate.read(&mut trailing)? != 0 {
                    anyhow::bail!("local CAS blob {candidate_hash} exceeds its declared size");
                }
                batch.push(BlobUpload {
                    data: base64::engine::general_purpose::STANDARD.encode(bytes),
                });
                expected.push(candidate_hash.clone());
                request_size = request_size.saturating_add(encoded_size);
                blob_index += 1;
            }
            let response = client
                .objects_put(Some(&staging_id), project_path_for_ref, &batch, &[])
                .await?;
            validate_upload_session(&response, &staging_id, expected_previous_hash.as_deref())?;
            validate_upload_response(&response.blob_hashes, &expected, "blob")?;
            validate_upload_response(&response.object_hashes, &[], "object")?;
            continue;
        }

        let mut offset = 0_u64;
        let mut buffer = vec![0_u8; crate::handlers::objects_put::MAX_BLOB_CHUNK_BYTES];
        while offset < total_size {
            let read = source.read(&mut buffer)?;
            if read == 0 {
                anyhow::bail!("local CAS blob {hash} ended before its declared size");
            }
            let next = offset
                .checked_add(u64::try_from(read)?)
                .ok_or_else(|| anyhow::anyhow!("blob upload offset overflow"))?;
            let chunk = BlobChunkUpload {
                hash: hash.clone(),
                total_size,
                offset,
                data: base64::engine::general_purpose::STANDARD.encode(&buffer[..read]),
            };
            let response = client
                .objects_put_blob_chunk(&staging_id, project_path_for_ref, &chunk)
                .await?;
            validate_upload_session(&response, &staging_id, expected_previous_hash.as_deref())?;
            validate_upload_response(&response.object_hashes, &[], "object")?;
            if validate_blob_chunk_response(&response.blob_hashes, hash, next == total_size)? {
                // A durable retry can begin again at offset zero after the
                // target already completed and verified this blob. In that
                // case the target returns the exact completed hash on the
                // first replayed chunk. Treat that acknowledgement as
                // authoritative and do not retransmit the remaining bytes.
                break;
            }
            offset = next;
        }
        blob_index += 1;
    }
    for batch in chunk_object_uploads(&objects)? {
        let expected = batch
            .iter()
            .map(|(hash, _)| hash.clone())
            .collect::<Vec<_>>();
        let values = batch
            .iter()
            .map(|(_, value)| value.clone())
            .collect::<Vec<_>>();
        let response = client
            .objects_put(Some(&staging_id), project_path_for_ref, &[], &values)
            .await?;
        validate_upload_session(&response, &staging_id, expected_previous_hash.as_deref())?;
        validate_upload_response(&response.blob_hashes, &[], "blob")?;
        validate_upload_response(&response.object_hashes, &expected, "object")?;
    }

    Ok(StagedUploadResult {
        staging_id,
        expected_previous_hash,
        uploaded,
        skipped,
    })
}

fn inline_blob_request_size(blob_size: u64) -> Result<usize> {
    let blob_size = usize::try_from(blob_size)?;
    Ok(blob_size
        .saturating_add(2)
        .checked_div(3)
        .unwrap_or(usize::MAX)
        .saturating_mul(4)
        .saturating_add(64))
}

fn chunk_object_uploads(
    entries: &[(String, serde_json::Value)],
) -> Result<Vec<&[(String, serde_json::Value)]>> {
    let mut encoded_sizes = Vec::with_capacity(entries.len());
    for (_, value) in entries {
        encoded_sizes.push(
            lillux::canonical_json(value)
                .context("failed to canonicalize object upload entry")?
                .len()
                .saturating_add(64),
        );
    }
    let mut index = 0;
    Ok(chunk_upload_entries(entries, |_| {
        let size = encoded_sizes[index];
        index += 1;
        size
    }))
}

fn chunk_upload_entries<T, F>(entries: &[T], mut encoded_size: F) -> Vec<&[T]>
where
    F: FnMut(&T) -> usize,
{
    let mut chunks = Vec::new();
    let mut start = 0;
    let mut size: usize = 256;
    for (index, entry) in entries.iter().enumerate() {
        let entry_size = encoded_size(entry);
        if index > start && size.saturating_add(entry_size) > OBJECTS_PUT_BODY_BUDGET_BYTES {
            chunks.push(&entries[start..index]);
            start = index;
            size = 256;
        }
        size = size.saturating_add(entry_size);
    }
    if start < entries.len() {
        chunks.push(&entries[start..]);
    }
    chunks
}

fn validate_upload_session(
    response: &ObjectsPutResponse,
    staging_id: &str,
    expected_previous_hash: Option<&str>,
) -> Result<()> {
    if response.staging_id != staging_id
        || response.expected_previous_hash.as_deref() != expected_previous_hash
    {
        anyhow::bail!("objects/put response changed the durable upload session contract");
    }
    Ok(())
}

fn validate_upload_response(actual: &[String], expected: &[String], kind: &str) -> Result<()> {
    if actual != expected {
        anyhow::bail!(
            "objects/put {kind} hash response mismatch: expected {:?}, got {:?}",
            expected,
            actual
        );
    }
    Ok(())
}

fn validate_blob_chunk_response(
    actual: &[String],
    expected_hash: &str,
    request_reached_end: bool,
) -> Result<bool> {
    match actual {
        [] if !request_reached_end => Ok(false),
        [hash] if hash == expected_hash => Ok(true),
        [] => anyhow::bail!(
            "objects/put blob hash response mismatch: expected completed hash {expected_hash}, got []"
        ),
        _ => anyhow::bail!(
            "objects/put blob hash response mismatch: expected [] or [{expected_hash:?}], got {:?}",
            actual
        ),
    }
}

/// Reject project paths that would walk the entire home directory
/// or contain the daemon's own app root.
///
/// Returns an error describing why and how to fix it. The error
/// message names the offending path so the operator can copy-paste a
/// corrected `-p` flag.
fn refuse_walking_root(project_path: &Path, app_root: &Path) -> Result<()> {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    refuse_walking_root_with_home(project_path, app_root, home.as_deref())
}

fn refuse_walking_root_with_home(
    project_path: &Path,
    app_root: &Path,
    home: Option<&Path>,
) -> Result<()> {
    // Canonicalise both so symlinks and `.` cannot bypass the protected-root
    // checks. This gate fails closed; a path we cannot identify exactly is not
    // safe to walk recursively.
    let proj = project_path
        .canonicalize()
        .with_context(|| format!("canonicalize project path {}", project_path.display()))?;
    let app_root_canon = app_root
        .canonicalize()
        .with_context(|| format!("canonicalize app root {}", app_root.display()))?;

    // Reject filebundle root
    if proj.parent().is_none() {
        anyhow::bail!(
            "refusing to push filebundle root '/'. \
             `remote execute` recursively ingests the project; \
             walking '/' would ingest the entire filesystem. \
             Re-run from inside a project directory, or pass \
             `-p <project-dir>` explicitly.",
        );
    }

    // $HOME comparison via env var to avoid a new dependency. The
    // env var is stable on every platform ryeos targets (Linux,
    // macOS). If $HOME is unset we just skip this check.
    if let Some(home) = home {
        let home = home
            .canonicalize()
            .with_context(|| format!("canonicalize HOME {}", home.display()))?;
        if proj == home {
            anyhow::bail!(
                "refusing to push {} — that's your home directory. \
                 `remote execute` recursively ingests the project; \
                 running it from $HOME would walk every file you own. \
                 Re-run from inside a project directory, or pass \
                 `-p <project-dir>` explicitly.",
                proj.display(),
            );
        }
    }

    if proj == app_root_canon
        || proj.starts_with(&app_root_canon)
        || app_root_canon.starts_with(&proj)
    {
        anyhow::bail!(
            "refusing to push {} — that path overlaps the daemon's \
             app root ({}). Pushing the daemon's own state to a \
             remote would corrupt both nodes. Re-run from a project \
             directory outside the daemon state tree.",
            proj.display(),
            app_root_canon.display(),
        );
    }

    Ok(())
}

#[cfg(test)]
mod refuse_walking_root_tests {
    use super::{refuse_walking_root, refuse_walking_root_with_home};
    use tempfile::TempDir;

    #[test]
    fn ordinary_project_dir_outside_home_passes() {
        // A tempdir under /tmp is neither $HOME nor inside the daemon
        // app root — must pass.
        let proj = TempDir::new().unwrap();
        let sys = TempDir::new().unwrap();
        refuse_walking_root(proj.path(), sys.path()).expect("ordinary dir must pass");
    }

    #[test]
    fn project_path_equal_to_home_is_refused() {
        let proj = TempDir::new().unwrap();
        let sys = TempDir::new().unwrap();
        let result = refuse_walking_root_with_home(proj.path(), sys.path(), Some(proj.path()));
        let err = result.expect_err("home dir must be refused");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("home directory"),
            "error must mention 'home directory', got: {msg}"
        );
    }

    #[test]
    fn project_path_inside_app_root_is_refused() {
        let sys = TempDir::new().unwrap();
        let inside = sys.path().join("inner");
        std::fs::create_dir_all(&inside).unwrap();
        let err = refuse_walking_root(&inside, sys.path())
            .expect_err("paths inside app root must be refused");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("app root"),
            "error must mention app root, got: {msg}"
        );
    }

    #[test]
    fn project_path_containing_app_root_is_refused() {
        // The inverse: project is the parent that contains the
        // app_root. Walking it would still hit the daemon
        // state.
        let proj = TempDir::new().unwrap();
        let sys = proj.path().join("daemon-state");
        std::fs::create_dir_all(&sys).unwrap();
        let err = refuse_walking_root(proj.path(), &sys)
            .expect_err("paths containing app root must be refused");
        let msg = format!("{err:#}");
        assert!(msg.contains("app root"), "got: {msg}");
    }

    #[test]
    fn nonexistent_path_fails_closed_before_walk() {
        // Recursive ingestion needs an exact path identity before any walk.
        // A path that cannot be canonicalised must fail at this authority
        // boundary instead of bypassing the protected-root checks.
        let sys = TempDir::new().unwrap();
        let project_parent = TempDir::new().unwrap();
        let missing = project_parent.path().join("missing-project");
        let err = refuse_walking_root(&missing, sys.path())
            .expect_err("unidentifiable project paths must fail closed");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("canonicalize project path")
                && msg.contains(&missing.display().to_string()),
            "error must identify the failed project-path canonicalisation, got: {msg}"
        );
    }

    // ── Item 9: missing refuse_walking_root coverage ──

    #[test]
    fn filesystem_root_is_refused() {
        // Walking '/' would ingest the entire filesystem. Must hard error.
        let sys = TempDir::new().unwrap();
        let err = refuse_walking_root(std::path::Path::new("/"), sys.path())
            .expect_err("filebundle root must be refused");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("filebundle root") || msg.contains("'/'"),
            "error must mention filebundle root, got: {msg}"
        );
    }
}

#[cfg(test)]
mod upload_batch_tests {
    use super::{
        OBJECTS_PUT_BODY_BUDGET_BYTES, inline_blob_request_size, validate_blob_chunk_response,
    };

    #[test]
    fn inline_blob_request_size_accounts_for_base64_and_entry_overhead() {
        assert_eq!(inline_blob_request_size(0).unwrap(), 64);
        assert_eq!(inline_blob_request_size(1).unwrap(), 68);
        assert_eq!(inline_blob_request_size(3).unwrap(), 68);
        assert_eq!(inline_blob_request_size(4).unwrap(), 72);
    }

    #[test]
    fn inline_blob_batch_boundary_stays_below_route_budget() {
        let largest_raw = (0..=OBJECTS_PUT_BODY_BUDGET_BYTES)
            .rev()
            .find(|size| {
                inline_blob_request_size(*size as u64).unwrap() <= OBJECTS_PUT_BODY_BUDGET_BYTES
            })
            .unwrap();
        assert!(
            inline_blob_request_size(largest_raw as u64).unwrap() <= OBJECTS_PUT_BODY_BUDGET_BYTES
        );
        assert!(
            inline_blob_request_size((largest_raw + 1) as u64).unwrap()
                > OBJECTS_PUT_BODY_BUDGET_BYTES
        );
    }

    #[test]
    fn blob_chunk_response_accepts_in_progress_and_terminal_acknowledgements() {
        let hash = "a".repeat(64);
        assert!(!validate_blob_chunk_response(&[], &hash, false).unwrap());
        assert!(validate_blob_chunk_response(std::slice::from_ref(&hash), &hash, true).unwrap());
    }

    #[test]
    fn blob_chunk_response_accepts_exact_early_completion_on_durable_retry() {
        let hash = "b".repeat(64);
        assert!(validate_blob_chunk_response(std::slice::from_ref(&hash), &hash, false).unwrap());
    }

    #[test]
    fn blob_chunk_response_requires_terminal_acknowledgement() {
        let hash = "c".repeat(64);
        let error = validate_blob_chunk_response(&[], &hash, true).unwrap_err();
        assert!(error.to_string().contains("expected completed hash"));
    }

    #[test]
    fn blob_chunk_response_rejects_wrong_or_ambiguous_hashes() {
        let hash = "d".repeat(64);
        let wrong = "e".repeat(64);
        assert!(validate_blob_chunk_response(std::slice::from_ref(&wrong), &hash, false).is_err());
        assert!(validate_blob_chunk_response(&[hash.clone(), hash.clone()], &hash, false).is_err());
    }
}
