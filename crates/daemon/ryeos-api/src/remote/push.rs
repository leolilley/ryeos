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

/// Build, upload, and stage an AI-only project snapshot.
///
/// Unlike [`push_project`], this captures the curated `.ai` allow-list rather
/// than a full-project tree. It still emits the current typed project snapshot
/// closure; excluded ordinary project paths are absent from that tree.
///
/// `remote_ignore`, when supplied, is applied to every candidate path
/// under the allow-list so files the remote would later reject (e.g.
/// `__pycache__/`, `*.pyc`) are dropped client-side instead of
/// blowing up at `/push-head`.
pub async fn push_project_ai_only(
    client: &RemoteClient,
    state: &Arc<AppState>,
    authority: &PinnedStateAuthority,
    local_project_path: &Path,
    remote_project_path_for_ref: &str,
    remote_ignore: Option<&IgnoreMatcher>,
) -> Result<PushResult> {
    let app_root = &state.config.app_root;
    refuse_walking_root(local_project_path, app_root)?;

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
        let empty;
        let matcher = match remote_ignore {
            Some(matcher) => matcher,
            None => {
                empty = IgnoreMatcher::from_config(&ryeos_state::ignore::IgnoreConfig {
                    patterns: Vec::new(),
                })?;
                &empty
            }
        };
        let policy = ryeos_state::project_sync::capture_snapshot_policy_from_pinned(
            &project_root,
            matcher,
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
            matcher,
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
/// 1. Apply the remote's ingest ignore rules to build the manifest
/// 2. Ingest locally into CAS
/// 3. Build manifest + snapshot
/// 4. Check which typed objects and blobs the remote already has
/// 5. Upload missing blobs and objects, including manifest + snapshot
/// 6. Call push-head to write the HEAD ref
///
/// The `remote_ignore` matcher is the **only** ignore policy used: the
/// manifest is built using the remote's rules so that the pushed content
/// matches what the remote would accept during ingest. Callers must
/// resolve ignore rules before calling this function.
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
            remote_ignore,
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
            remote_ignore,
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
    match (operation, finish) {
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

pub(crate) fn collect_snapshot_hashes(
    cas: &CasStore,
    snapshot_hash: &str,
) -> Result<SnapshotCasClosure> {
    let limits = ryeos_state::object_closure::ObjectClosureLimits::for_project_snapshot_transport();
    let report = ryeos_state::object_closure::collect_object_closure_with_cas_and_limits(
        cas,
        [snapshot_hash.to_string()],
        limits,
    )?;
    if !report.is_complete() {
        anyhow::bail!("project snapshot CAS closure is incomplete: {report:?}");
    }
    Ok(SnapshotCasClosure {
        object_hashes: report.object_hashes.into_iter().collect(),
        blob_hashes: report.blob_hashes.into_iter().collect(),
    })
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
    if snapshot.parent_hashes != expected_parents {
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

    // Stream each immutable blob through bounded, sequential, retry-idempotent
    // chunks. Peak memory is independent of both blob size and generation
    // size; the remote verifies the complete digest before admitting the blob
    // to this publication capability.
    for hash in &missing_blobs {
        let (mut source, total_size) = local_cas
            .open_blob(hash)?
            .ok_or_else(|| anyhow::anyhow!("local CAS blob {hash} disappeared before upload"))?;
        if total_size > limits.max_blob_bytes {
            anyhow::bail!(
                "project blob {hash} exceeds transport limit: {total_size} > {}",
                limits.max_blob_bytes
            );
        }
        if total_size == 0 {
            let response = client
                .objects_put(
                    Some(&staging_id),
                    project_path_for_ref,
                    &[BlobUpload {
                        data: String::new(),
                    }],
                    &[],
                )
                .await?;
            validate_upload_session(&response, &staging_id, expected_previous_hash.as_deref())?;
            validate_upload_response(&response.blob_hashes, std::slice::from_ref(hash), "blob")?;
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
            let expected = if next == total_size {
                std::slice::from_ref(hash)
            } else {
                &[]
            };
            validate_upload_response(&response.blob_hashes, expected, "blob")?;
            validate_upload_response(&response.object_hashes, &[], "object")?;
            offset = next;
        }
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

/// Reject project paths that would walk the entire home directory
/// or contain the daemon's own app root.
///
/// Returns an error describing why and how to fix it. The error
/// message names the offending path so the operator can copy-paste a
/// corrected `-p` flag.
fn refuse_walking_root(project_path: &Path, app_root: &Path) -> Result<()> {
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
    if let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) {
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
    use super::refuse_walking_root;
    use tempfile::TempDir;

    /// Tests in this module mutate `$HOME`; serialize those mutations.
    static HOME_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
        // Spoof $HOME to point at the project dir; the function
        // must catch that and refuse with a clear $HOME message.
        // SAFETY: serialised via HOME_ENV_LOCK to prevent concurrent
        // HOME-mutating tests in this module from racing.
        let _guard = HOME_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let old_home = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", proj.path());
        }
        let result = refuse_walking_root(proj.path(), sys.path());
        unsafe {
            match old_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
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
