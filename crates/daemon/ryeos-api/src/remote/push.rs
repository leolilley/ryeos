//! CAS push pipeline for remote nodes.
//!
//! Handles the ingest-locally → upload-blobs → push-head pipeline
//! for pushing project content to a remote node.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use base64::Engine as _;

use lillux::cas::{sha256_hex, CasStore};
use ryeos_state::ignore::IgnoreMatcher;
use ryeos_state::objects::{ItemSource, ProjectSnapshot, SourceManifest};
use ryeos_state::project_sync::{ProjectSyncScope, PROJECT_AI_SURFACES};
use ryeos_state::{CasMutationGuard, PinnedStateAuthority, StagedCasRootLease};

use crate::remote::client::{BlobUpload, ObjectsPutResponse, RemoteClient};
use ryeos_app::state::AppState;

const OBJECTS_PUT_BODY_BUDGET_BYTES: usize = 900 * 1024;

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
    /// Operator manifest hash, if the operator's app root had any
    /// content under the sync allow-list. Used by pull_results() as
    /// the symmetric base for operator-side diff/apply.
    pub user_manifest_hash: Option<String>,
    /// The exact pushed user manifest. None when the operator's user
    /// space is empty / absent.
    pub user_manifest: Option<SourceManifest>,
}

/// Build, upload, and stage an AI-only project snapshot.
///
/// Unlike [`push_project`], this is allow-list based and project-only:
/// it does not ingest app files and never includes a user manifest.
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
    let mut staged_roots = {
        let guard = authority.acquire_shared_guard()?;
        recovery.begin_staged_cas_roots_admitted(&guard, "remote-push-ai")?
    };

    let operation = async {
        let mut items: HashMap<String, String> = HashMap::new();
        let manifest_hash;
        let manifest;
        {
            let guard = authority.acquire_shared_guard()?;
            let mut staging = StagedCasWriter::new(&mut staged_roots, &guard);
            ingest_project_ai_for_push(
                &local_cas,
                local_project_path,
                &mut items,
                remote_ignore,
                Some(&mut staging),
            )?;

            manifest = SourceManifest {
                item_source_hashes: items,
            };
            ryeos_state::project_sync::validate_project_manifest_paths(
                &manifest,
                ProjectSyncScope::AiOnly,
                remote_ignore,
            )?;
            manifest_hash = staging.store_object(&local_cas, &manifest.to_value())?;
        }

        let upload_session = client
            .objects_put(None, remote_project_path_for_ref, &[], &[])
            .await?;

        let snapshot = ryeos_state::objects::ProjectSnapshot {
            project_manifest_hash: manifest_hash.clone(),
            user_manifest_hash: None,
            message: None,
            project_sync_scope: ProjectSyncScope::AiOnly,
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
            let upload_closure = collect_snapshot_hashes(
                &local_cas,
                &manifest,
                None,
                None,
                &manifest_hash,
                &snapshot_hash,
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

        let manifest_entries = manifest.item_source_hashes.len();
        Ok(PushResult {
            snapshot_hash,
            manifest_hash,
            manifest,
            manifest_entries,
            blobs_uploaded: upload.uploaded,
            blobs_skipped: upload.skipped,
            user_manifest_hash: None,
            user_manifest: None,
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
    let mut staged_roots = {
        let guard = authority.acquire_shared_guard()?;
        recovery.begin_staged_cas_roots_admitted(&guard, "remote-push-project")?
    };

    let operation = async {
        let mut items: HashMap<String, String> = HashMap::new();
        let manifest_hash;
        let manifest;
        {
            let guard = authority.acquire_shared_guard()?;
            let mut staging = StagedCasWriter::new(&mut staged_roots, &guard);
            ingest_for_push(
                &local_cas,
                project_path,
                project_path,
                &mut items,
                remote_ignore,
                Some(&mut staging),
            )?;

            manifest = SourceManifest {
                item_source_hashes: items,
            };
            manifest_hash = staging.store_object(&local_cas, &manifest.to_value())?;
        }

        let upload_session = client
            .objects_put(None, project_path_for_ref, &[], &[])
            .await?;

        let snapshot = ryeos_state::objects::ProjectSnapshot {
            project_manifest_hash: manifest_hash.clone(),
            user_manifest_hash: None,
            message: None,
            project_sync_scope: ryeos_state::project_sync::ProjectSyncScope::FullProject,
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
            let upload_closure = collect_snapshot_hashes(
                &local_cas,
                &manifest,
                None,
                None,
                &manifest_hash,
                &snapshot_hash,
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

        let manifest_entries = manifest.item_source_hashes.len();
        Ok(PushResult {
            snapshot_hash,
            manifest_hash,
            manifest,
            manifest_entries,
            blobs_uploaded: upload.uploaded,
            blobs_skipped: upload.skipped,
            user_manifest_hash: None,
            user_manifest: None,
        })
    }
    .await;

    finish_staged_roots(authority, &mut staged_roots, operation)
}

struct StagedCasWriter<'a> {
    roots: &'a mut StagedCasRootLease,
    guard: &'a CasMutationGuard,
}

impl<'a> StagedCasWriter<'a> {
    fn new(roots: &'a mut StagedCasRootLease, guard: &'a CasMutationGuard) -> Self {
        Self { roots, guard }
    }

    fn store_object(&mut self, cas: &CasStore, value: &serde_json::Value) -> Result<String> {
        self.roots.store_object_admitted(self.guard, cas, value)
    }

    fn store_blob(&mut self, cas: &CasStore, bytes: &[u8]) -> Result<String> {
        self.roots.store_blob_admitted(self.guard, cas, bytes)
    }
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
    manifest: &SourceManifest,
    user_manifest: Option<&SourceManifest>,
    user_manifest_hash: Option<&str>,
    manifest_hash: &str,
    snapshot_hash: &str,
) -> Result<SnapshotCasClosure> {
    let limits = ryeos_state::object_closure::ObjectClosureLimits::default();
    let stored_manifest = ryeos_state::object_closure::load_exact_cas_object_with_cas(
        cas,
        manifest_hash,
        limits.max_object_bytes,
    )?;
    if stored_manifest != manifest.to_value() {
        anyhow::bail!("project manifest hash does not identify the supplied manifest");
    }
    let snapshot_value = ryeos_state::object_closure::load_exact_cas_object_with_cas(
        cas,
        snapshot_hash,
        limits.max_object_bytes,
    )?;
    let snapshot = ProjectSnapshot::from_value(&snapshot_value)?;
    if snapshot.project_manifest_hash != manifest_hash
        || snapshot.user_manifest_hash.as_deref() != user_manifest_hash
    {
        anyhow::bail!("snapshot manifest links do not match the supplied upload closure");
    }
    let mut object_hashes = BTreeSet::new();
    let mut blob_hashes = BTreeSet::new();
    collect_manifest_hashes(cas, manifest, &mut object_hashes, &mut blob_hashes)?;
    if let Some(um) = user_manifest {
        let user_hash = user_manifest_hash
            .ok_or_else(|| anyhow::anyhow!("user manifest was supplied without its CAS hash"))?;
        let stored_user_manifest = ryeos_state::object_closure::load_exact_cas_object_with_cas(
            cas,
            user_hash,
            limits.max_object_bytes,
        )?;
        if stored_user_manifest != um.to_value() {
            anyhow::bail!("user manifest hash does not identify the supplied manifest");
        }
        collect_manifest_hashes(cas, um, &mut object_hashes, &mut blob_hashes)?;
    } else if user_manifest_hash.is_some() {
        anyhow::bail!("user manifest hash was supplied without the typed manifest");
    }
    if let Some(umh) = user_manifest_hash {
        object_hashes.insert(umh.to_string());
    }
    object_hashes.insert(manifest_hash.to_string());
    object_hashes.insert(snapshot_hash.to_string());
    Ok(SnapshotCasClosure {
        object_hashes: object_hashes.into_iter().collect(),
        blob_hashes: blob_hashes.into_iter().collect(),
    })
}

fn collect_manifest_hashes(
    cas: &CasStore,
    manifest: &SourceManifest,
    object_hashes: &mut BTreeSet<String>,
    blob_hashes: &mut BTreeSet<String>,
) -> Result<()> {
    let limits = ryeos_state::object_closure::ObjectClosureLimits::default();
    for (path, obj_hash) in &manifest.item_source_hashes {
        object_hashes.insert(obj_hash.clone());
        let item_obj = ryeos_state::object_closure::load_exact_cas_object_with_cas(
            cas,
            obj_hash,
            limits.max_object_bytes,
        )?;
        let item = ItemSource::from_value(&item_obj)?;
        if item.item_ref != *path {
            anyhow::bail!(
                "manifest path '{}' does not match item_source identity '{}'",
                path,
                item.item_ref
            );
        }
        ryeos_state::object_closure::load_exact_cas_blob_with_cas(
            cas,
            &item.content_blob_hash,
            limits.max_blob_bytes,
        )?;
        if item.integrity != item.content_blob_hash {
            anyhow::bail!(
                "item_source '{}' integrity does not match its content blob hash",
                path
            );
        }
        blob_hashes.insert(item.content_blob_hash);
    }
    Ok(())
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

    let mut blobs = Vec::new();
    let mut objects = Vec::new();
    let mut publication_root_staged = false;
    let limits = ryeos_state::object_closure::ObjectClosureLimits::default();

    for hash in &missing_blobs {
        let data = ryeos_state::object_closure::load_exact_cas_blob_with_cas(
            local_cas,
            hash,
            limits.max_blob_bytes,
        )?;
        let encoded = base64::engine::general_purpose::STANDARD.encode(&data);
        blobs.push((hash.clone(), BlobUpload { data: encoded }));
    }
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
    blobs.sort_by(|left, right| left.0.cmp(&right.0));
    objects.sort_by(|left, right| left.0.cmp(&right.0));
    objects.dedup_by(|left, right| left.0 == right.0);

    let staging_id = upload_session.staging_id;
    let expected_previous_hash = upload_session.expected_previous_hash;
    validate_upload_response(&upload_session.blob_hashes, &[], "initial blob")?;
    validate_upload_response(&upload_session.object_hashes, &[], "initial object")?;

    for batch in chunk_blob_uploads(&blobs) {
        let expected = batch
            .iter()
            .map(|(hash, _)| hash.clone())
            .collect::<Vec<_>>();
        let values = batch
            .iter()
            .map(|(_, value)| value.clone())
            .collect::<Vec<_>>();
        let response = client
            .objects_put(Some(&staging_id), project_path_for_ref, &values, &[])
            .await?;
        validate_upload_session(&response, &staging_id, expected_previous_hash.as_deref())?;
        validate_upload_response(&response.blob_hashes, &expected, "blob")?;
        validate_upload_response(&response.object_hashes, &[], "object")?;
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

fn chunk_blob_uploads(entries: &[(String, BlobUpload)]) -> Vec<&[(String, BlobUpload)]> {
    chunk_upload_entries(entries, |(_, upload)| upload.data.len().saturating_add(64))
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
    let mut size = 256;
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

/// Walk only managed project AI roots and ingest regular files.
/// Symlinked allow-list roots and symlinks inside those roots are skipped.
fn ingest_project_ai_for_push(
    cas: &CasStore,
    project_root: &Path,
    items: &mut HashMap<String, String>,
    remote_ignore: Option<&IgnoreMatcher>,
    mut staging: Option<&mut StagedCasWriter<'_>>,
) -> Result<()> {
    for surface in PROJECT_AI_SURFACES
        .iter()
        .filter(|surface| surface.materialize_to_project)
    {
        let abs = project_root.join(surface.root);
        match std::fs::symlink_metadata(&abs) {
            Ok(md) if md.file_type().is_symlink() => {
                tracing::warn!(path = %abs.display(), "skipping symlinked project AI sync root");
                continue;
            }
            Ok(md) if md.is_dir() => ingest_project_ai_dir_for_push(
                cas,
                project_root,
                &abs,
                items,
                remote_ignore,
                staging.as_deref_mut(),
            )?,
            _ => continue,
        }
    }
    Ok(())
}

fn exact_relative_project_path(root: &Path, path: &Path) -> Result<String> {
    let relative = path.strip_prefix(root).with_context(|| {
        format!(
            "project path '{}' escaped root '{}'",
            path.display(),
            root.display()
        )
    })?;
    Ok(relative
        .to_str()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "project-relative path '{}' is not valid UTF-8",
                relative.display()
            )
        })?
        .replace('\\', "/"))
}

fn ingest_project_ai_dir_for_push(
    cas: &CasStore,
    project_root: &Path,
    dir: &Path,
    items: &mut HashMap<String, String>,
    remote_ignore: Option<&IgnoreMatcher>,
    mut staging: Option<&mut StagedCasWriter<'_>>,
) -> Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };
    for entry in entries {
        let entry = entry?;
        let ft = entry.file_type()?;
        if ft.is_symlink() {
            tracing::warn!(path = %entry.path().display(), "skipping symlink in project AI ingest");
            continue;
        }
        let path = entry.path();
        if ft.is_dir() {
            if let Some(matcher) = remote_ignore {
                let rel_dir = exact_relative_project_path(project_root, &path)?;
                if matcher.is_ignored(&rel_dir) {
                    continue;
                }
            }
            ingest_project_ai_dir_for_push(
                cas,
                project_root,
                &path,
                items,
                remote_ignore,
                staging.as_deref_mut(),
            )?;
        } else if ft.is_file() {
            let rel = exact_relative_project_path(project_root, &path)?;
            if let Some(matcher) = remote_ignore {
                if matcher.is_ignored(&rel) {
                    continue;
                }
            }
            let bytes = std::fs::read(&path)?;
            let blob_hash = match staging.as_deref_mut() {
                Some(staging) => staging.store_blob(cas, &bytes)?,
                None => cas.store_blob(&bytes)?,
            };
            let integrity = sha256_hex(&bytes);
            let item_source = ryeos_state::objects::ItemSource {
                item_ref: rel.clone(),
                content_blob_hash: blob_hash,
                integrity,
                signature_info: None,
                mode: executable_mode(&path),
            };
            let item_value = item_source.to_value();
            let obj_hash = match staging.as_deref_mut() {
                Some(staging) => staging.store_object(cas, &item_value)?,
                None => cas.store_object(&item_value)?,
            };
            items.insert(rel, obj_hash);
        }
    }
    Ok(())
}

fn executable_mode(path: &Path) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .ok()
            .map(|m| m.permissions().mode() & 0o7777)
            .filter(|m| m & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

/// Walk a project directory and ingest files for push.
fn ingest_for_push(
    cas: &CasStore,
    root: &Path,
    dir: &Path,
    items: &mut HashMap<String, String>,
    ignore: &IgnoreMatcher,
    mut staging: Option<&mut StagedCasWriter<'_>>,
) -> Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };

    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let rel = exact_relative_project_path(root, &path)?;

        // Skip state/ directory
        if rel.starts_with("state/") || rel == "state" {
            continue;
        }

        // Apply ignore rules
        if ignore.is_ignored(&rel) {
            continue;
        }

        if path.is_dir() {
            ingest_for_push(cas, root, &path, items, ignore, staging.as_deref_mut())?;
        } else if path.is_file() {
            let bytes = std::fs::read(&path)?;
            let blob_hash = match staging.as_deref_mut() {
                Some(staging) => staging.store_blob(cas, &bytes)?,
                None => cas.store_blob(&bytes)?,
            };
            let integrity = sha256_hex(&bytes);

            let item_source = ryeos_state::objects::ItemSource {
                item_ref: rel.clone(),
                content_blob_hash: blob_hash,
                integrity,
                signature_info: None,
                mode: None,
            };
            let item_value = item_source.to_value();
            let obj_hash = match staging.as_deref_mut() {
                Some(staging) => staging.store_object(cas, &item_value)?,
                None => cas.store_object(&item_value)?,
            };
            items.insert(rel, obj_hash);
        }
    }
    Ok(())
}

#[cfg(test)]
mod project_ai_ingest_tests {
    use super::ingest_project_ai_for_push;
    use lillux::cas::CasStore;
    use std::collections::HashMap;
    use tempfile::TempDir;

    #[test]
    fn project_ai_ingest_includes_schedule_declarations_not_node_schedule_specs() {
        let project = TempDir::new().unwrap();
        let cas_dir = TempDir::new().unwrap();
        let cas = CasStore::new(cas_dir.path().to_path_buf());
        std::fs::create_dir_all(project.path().join(".ai/config/schedules")).unwrap();
        std::fs::create_dir_all(project.path().join(".ai/node/schedules")).unwrap();
        std::fs::write(
            project.path().join(".ai/config/schedules/snap-track.yaml"),
            "schedule intent",
        )
        .unwrap();
        std::fs::write(
            project.path().join(".ai/node/schedules/runtime.yaml"),
            "runtime state",
        )
        .unwrap();

        let mut items = HashMap::new();
        ingest_project_ai_for_push(&cas, project.path(), &mut items, None, None).unwrap();

        assert!(items.contains_key(".ai/config/schedules/snap-track.yaml"));
        assert!(!items.contains_key(".ai/node/schedules/runtime.yaml"));
    }
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
    fn nonexistent_path_does_not_short_circuit() {
        // Canonicalisation fails for nonexistent paths; the guard
        // must skip rather than spuriously refuse — the downstream
        // walk will produce a clearer "not found" error.
        let sys = TempDir::new().unwrap();
        let missing = std::path::Path::new("/this/does/not/exist/anywhere");
        refuse_walking_root(missing, sys.path())
            .expect("missing path must pass the guard (fail elsewhere)");
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
mod ingest_symlink_tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn ingest_project_ai_walks_only_managed_roots() {
        let cas_dir = TempDir::new().unwrap();
        let cas = CasStore::new(cas_dir.path().to_path_buf());
        let project = TempDir::new().unwrap();

        std::fs::create_dir_all(project.path().join(".ai/directives")).unwrap();
        std::fs::create_dir_all(project.path().join(".ai/state")).unwrap();
        std::fs::create_dir_all(project.path().join("src")).unwrap();
        std::fs::write(project.path().join(".ai/directives/ok.md"), "ok").unwrap();
        std::fs::write(project.path().join(".ai/state/runtime.sqlite3"), "state").unwrap();
        std::fs::write(project.path().join("src/index.ts"), "app").unwrap();

        let mut items = HashMap::new();
        ingest_project_ai_for_push(&cas, project.path(), &mut items, None, None).unwrap();

        assert!(items.contains_key(".ai/directives/ok.md"));
        assert!(
            !items.keys().any(|k| k.contains("runtime.sqlite3")),
            "state must not be ingested: {items:?}"
        );
        assert!(
            !items.keys().any(|k| k.starts_with("src/")),
            "app code must not be ingested: {items:?}"
        );
    }

    #[test]
    fn ingest_project_ai_skips_symlinks_and_preserves_exec_mode() {
        #[cfg(not(unix))]
        {
            return;
        }

        use std::os::unix::fs::PermissionsExt;

        let cas_dir = TempDir::new().unwrap();
        let cas = CasStore::new(cas_dir.path().to_path_buf());
        let project = TempDir::new().unwrap();
        let tools = project.path().join(".ai/tools");
        std::fs::create_dir_all(&tools).unwrap();
        let tool = tools.join("run.sh");
        std::fs::write(&tool, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();

        let outside = TempDir::new().unwrap();
        std::fs::write(outside.path().join("secret"), "secret").unwrap();
        std::os::unix::fs::symlink(outside.path().join("secret"), tools.join("leak.sh")).unwrap();

        let mut items = HashMap::new();
        ingest_project_ai_for_push(&cas, project.path(), &mut items, None, None).unwrap();
        assert!(items.contains_key(".ai/tools/run.sh"));
        assert!(!items.contains_key(".ai/tools/leak.sh"));

        let obj_hash = items.get(".ai/tools/run.sh").unwrap();
        let obj = cas.get_object(obj_hash).unwrap().unwrap();
        let item = ryeos_state::objects::ItemSource::from_value(&obj).unwrap();
        assert_eq!(item.mode.map(|m| m & 0o777), Some(0o755));
    }
}
