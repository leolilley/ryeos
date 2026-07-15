//! Daemon-authoritative project snapshot reads and mutation.
//!
//! The terminal tool is only a callback client. It never loads the node key,
//! invents a trust store, reads authoritative refs, or publishes HEAD itself.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fs;
#[cfg(unix)]
use std::fs::{File, OpenOptions};
#[cfg(unix)]
use std::io::Read;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
#[cfg(unix)]
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use lillux::cas::{sha256_hex, CasStore};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::callback_token::{CallbackCapability, ThreadAuthState};
use crate::state::AppState;
use crate::state_store::NodeIdentitySigner;
use ryeos_runtime::authorizer::AuthorizationPolicy;
use ryeos_state::objects::{ItemSource, ProjectSnapshot, SourceManifest};
use ryeos_state::project_sync::ProjectSyncScope;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeProjectSnapshotRequest {
    pub thread_id: String,
    pub operation: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Deserialize)]
struct ProjectParams {
    #[serde(default)]
    project_path: Option<PathBuf>,
    #[serde(default)]
    include_unchanged: bool,
    #[serde(default)]
    time_budget_ms: u64,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    allow_empty: bool,
    #[serde(default)]
    snapshot_hash: Option<String>,
}

struct SnapshotContext<'a> {
    state: &'a AppState,
    project_path: PathBuf,
    project_hash: String,
    principal_key: String,
    cas: CasStore,
}

pub struct RuntimeProjectSnapshotService;

impl RuntimeProjectSnapshotService {
    pub fn execute(
        state: &AppState,
        cap: &CallbackCapability,
        thread_auth: &ThreadAuthState,
        request: RuntimeProjectSnapshotRequest,
    ) -> Result<Value> {
        if request.thread_id != cap.thread_id || request.thread_id != thread_auth.thread_id {
            bail!("snapshot callback authority does not match the running thread");
        }
        let required_capability = match request.operation.as_str() {
            "status" | "log" | "show" => "project.read",
            "create" => "project.write",
            other => bail!("unsupported snapshot operation: {other}"),
        };
        state
            .authorizer
            .authorize(
                &cap.effective_caps,
                &AuthorizationPolicy::require(required_capability),
            )
            .with_context(|| {
                format!(
                    "missing required capability: {required_capability} — project snapshots are daemon-mediated runtime authority"
                )
            })?;
        let params: ProjectParams =
            serde_json::from_value(request.params).context("invalid snapshot params")?;
        let project_path = canonical_project_path(
            params
                .project_path
                .as_deref()
                .unwrap_or(cap.project_path.as_path()),
        )?;
        let capability_path = canonical_project_path(&cap.project_path)?;
        if project_path != capability_path {
            bail!("snapshot project_path is outside the callback capability");
        }
        let canonical = project_path
            .to_str()
            .ok_or_else(|| anyhow!("canonical project_path is not valid UTF-8"))?;
        let ctx = SnapshotContext {
            state,
            project_hash: ryeos_state::refs::deployed_project_key(canonical),
            principal_key: ryeos_state::refs::principal_storage_key(&thread_auth.acting_principal)?
                .to_owned(),
            cas: state.cas_store()?,
            project_path,
        };
        match request.operation.as_str() {
            "status" => status(&ctx, &params),
            "log" => log(&ctx, params.limit.unwrap_or(20).max(1)),
            "create" => create(&ctx, params.message, params.allow_empty),
            "show" => show(
                &ctx,
                params
                    .snapshot_hash
                    .as_deref()
                    .ok_or_else(|| anyhow!("snapshot_hash is required"))?,
            ),
            _ => unreachable!("operation was validated before authorization"),
        }
    }
}

fn heads(ctx: &SnapshotContext<'_>) -> Result<(Option<String>, Option<String>)> {
    ctx.state.state_store.with_state_db(|db| {
        Ok((
            db.read_project_head(&ctx.principal_key, &ctx.project_hash)?,
            db.read_deployed_project_ref(&ctx.project_hash)?
                .map(|head| head.target_hash),
        ))
    })
}

fn status(ctx: &SnapshotContext<'_>, params: &ProjectParams) -> Result<Value> {
    let _cas_guard = acquire_cas_read_guard(ctx)?;
    let started = Instant::now();
    let deadline =
        (params.time_budget_ms > 0).then(|| started + Duration::from_millis(params.time_budget_ms));
    let (head_hash, deployed_hash) = heads(ctx)?;
    let head_items = match head_hash.as_deref() {
        Some(hash) => {
            load_snapshot_and_manifest(&ctx.cas, hash)?
                .1
                .item_source_hashes
        }
        None => HashMap::new(),
    };
    let head_state = manifest_state_map(&ctx.cas, &head_items)?;
    let mut worktree = BTreeMap::new();
    let complete = walk_worktree(
        &ctx.project_path,
        &ctx.project_path,
        Path::new(""),
        ctx.state.ignore_matcher.as_ref(),
        deadline,
        &mut worktree,
    )?;
    let mut paths = BTreeSet::new();
    paths.extend(worktree.keys().cloned());
    if complete {
        paths.extend(head_items.keys().cloned());
    }
    let mut counts = BTreeMap::from([
        ("added", 0_usize),
        ("modified", 0),
        ("deleted", 0),
        ("unchanged", 0),
    ]);
    let mut changes = Vec::new();
    for path in paths {
        let head = head_state.get(&path);
        let work = worktree.get(&path).map(|captured| &captured.state);
        let status = match (head, work) {
            (None, Some(_)) => "added",
            (Some(_), None) => "deleted",
            (Some(a), Some(b)) if a != b => "modified",
            (Some(_), Some(_)) => "unchanged",
            (None, None) => continue,
        };
        *counts.get_mut(status).expect("fixed status key") += 1;
        if status != "unchanged" || params.include_unchanged {
            changes.push(json!({
                "path": path.clone(),
                "status": status,
                "head_item_source_hash": head_items.get(&path),
                "worktree_integrity": work.map(|state| state.integrity.as_str()),
            }));
        }
    }
    Ok(json!({
        "kind": "snapshot_status",
        "project_path": ctx.project_path,
        "project_hash": ctx.project_hash,
        "principal_key": ctx.principal_key,
        "baseline": "principal_head",
        "head_snapshot_hash": head_hash,
        "deployed_snapshot_hash": deployed_hash,
        "dirty": counts["added"] > 0 || counts["modified"] > 0 || counts["deleted"] > 0,
        "scan_complete": complete,
        "scan_elapsed_ms": started.elapsed().as_millis() as u64,
        "counts": counts,
        "changes": changes,
    }))
}

fn log(ctx: &SnapshotContext<'_>, limit: usize) -> Result<Value> {
    let _cas_guard = acquire_cas_read_guard(ctx)?;
    let (head, _) = heads(ctx)?;
    let mut pending = BTreeSet::new();
    if let Some(head) = head.as_ref() {
        pending.insert((0_usize, head.clone()));
    }
    let mut seen = HashSet::new();
    let traversal_limits = ryeos_state::object_closure::ObjectClosureLimits::default();
    let mut entries = Vec::new();
    while entries.len() < limit {
        let Some((depth, hash)) = pending.pop_first() else {
            break;
        };
        if !seen.insert(hash.clone()) {
            continue;
        }
        let snapshot = load_snapshot(&ctx.cas, &hash)?;
        let mut parents = snapshot.parent_hashes.clone();
        if parents.len() > traversal_limits.max_links_per_object {
            bail!("snapshot has too many parent links");
        }
        parents.sort();
        for parent in parents {
            if !seen.contains(&parent) {
                pending.insert((depth.saturating_add(1), parent));
            }
        }
        entries.push(json!({
            "snapshot_hash": hash,
            "depth": depth,
            "created_at": snapshot.created_at,
            "source": snapshot.source,
            "message": snapshot.message,
            "project_manifest_hash": snapshot.project_manifest_hash,
            "parent_hashes": snapshot.parent_hashes,
        }));
    }
    Ok(json!({
        "kind": "snapshot_log",
        "project_path": ctx.project_path,
        "project_hash": ctx.project_hash,
        "principal_key": ctx.principal_key,
        "head_snapshot_hash": head,
        "entries": entries,
    }))
}

fn create(ctx: &SnapshotContext<'_>, message: Option<String>, allow_empty: bool) -> Result<Value> {
    let guard =
        ryeos_state::CasMutationGuard::acquire_shared(&ctx.state.config.runtime_state_dir())?;
    guard.ensure_protects_cas_root(ctx.cas.root())?;
    let _permit = ctx
        .state
        .write_barrier
        .try_acquire()
        .map_err(|error| anyhow!("cannot acquire snapshot write permit: {error}"))?;
    let (initial_head, _) = heads(ctx)?;
    let current = initial_head
        .as_deref()
        .map(|hash| load_snapshot(&ctx.cas, hash))
        .transpose()?;
    let manifest = build_manifest(ctx)?;
    let manifest_hash = ctx.cas.store_object(&manifest.to_value())?;
    if !allow_empty
        && current
            .as_ref()
            .is_some_and(|snapshot| snapshot.project_manifest_hash == manifest_hash)
    {
        return Ok(json!({
            "kind": "snapshot_create",
            "project_path": ctx.project_path,
            "project_hash": ctx.project_hash,
            "principal_key": ctx.principal_key,
            "created": false,
            "reason": "clean",
            "head_snapshot_hash": initial_head,
            "manifest_hash": manifest_hash,
            "manifest_entries": manifest.item_source_hashes.len(),
            "message": message,
        }));
    }
    let parent_hashes = initial_head.iter().cloned().collect::<Vec<_>>();
    let snapshot = ProjectSnapshot {
        project_manifest_hash: manifest_hash.clone(),
        user_manifest_hash: None,
        message: message.clone(),
        project_sync_scope: current
            .as_ref()
            .map(|snapshot| snapshot.project_sync_scope)
            .unwrap_or(ProjectSyncScope::FullProject),
        parent_hashes: parent_hashes.clone(),
        created_at: lillux::time::iso8601_now(),
        source: "snapshot_create".to_string(),
    };
    let snapshot_hash = ctx.cas.store_object(&snapshot.to_value())?;
    let signer = NodeIdentitySigner::from_identity(&ctx.state.identity);
    ctx.state.state_store.with_state_db(|db| {
        let locked_head = db.read_project_head(&ctx.principal_key, &ctx.project_hash)?;
        if locked_head != initial_head {
            bail!("project head changed while creating snapshot; rerun snapshot create");
        }
        match initial_head.as_deref() {
            Some(current) => db.advance_project_head_ref(
                &ctx.principal_key,
                &ctx.project_hash,
                &snapshot_hash,
                current,
                &signer,
            ),
            None => db.write_project_head_ref(
                &ctx.principal_key,
                &ctx.project_hash,
                &snapshot_hash,
                &signer,
            ),
        }
    })?;
    Ok(json!({
        "kind": "snapshot_create",
        "project_path": ctx.project_path,
        "project_hash": ctx.project_hash,
        "principal_key": ctx.principal_key,
        "created": true,
        "snapshot_hash": snapshot_hash,
        "head_snapshot_hash": snapshot_hash,
        "parent_hashes": parent_hashes,
        "manifest_hash": manifest_hash,
        "manifest_entries": manifest.item_source_hashes.len(),
        "message": message,
    }))
}

fn show(ctx: &SnapshotContext<'_>, snapshot_hash: &str) -> Result<Value> {
    let _cas_guard = acquire_cas_read_guard(ctx)?;
    let (principal_head, deployed_head) = heads(ctx)?;
    let in_principal_history =
        history_contains(&ctx.cas, principal_head.as_deref(), snapshot_hash)?;
    let in_deployed_history = history_contains(&ctx.cas, deployed_head.as_deref(), snapshot_hash)?;
    if !in_principal_history && !in_deployed_history {
        bail!("snapshot is not reachable from this project's verified heads");
    }
    let (snapshot, manifest) = load_snapshot_and_manifest(&ctx.cas, snapshot_hash)?;
    Ok(json!({
        "kind": "snapshot_show",
        "snapshot_hash": snapshot_hash,
        "created_at": snapshot.created_at,
        "source": snapshot.source,
        "message": snapshot.message,
        "project_manifest_hash": snapshot.project_manifest_hash,
        "project_sync_scope": snapshot.project_sync_scope,
        "parent_hashes": snapshot.parent_hashes,
        "manifest_entries": manifest.item_source_hashes.len(),
        "is_principal_head": principal_head.as_deref() == Some(snapshot_hash),
        "is_deployed": deployed_head.as_deref() == Some(snapshot_hash),
    }))
}

fn history_contains(cas: &CasStore, head: Option<&str>, wanted: &str) -> Result<bool> {
    let mut pending = VecDeque::new();
    if let Some(head) = head {
        pending.push_back(head.to_string());
    }
    let mut seen = HashSet::new();
    let max_snapshots = ryeos_state::object_closure::ObjectClosureLimits::default().max_objects;
    let max_parents =
        ryeos_state::object_closure::ObjectClosureLimits::default().max_links_per_object;
    while let Some(hash) = pending.pop_front() {
        if hash == wanted {
            return Ok(true);
        }
        if !seen.insert(hash.clone()) {
            continue;
        }
        if seen.len() > max_snapshots {
            bail!("snapshot history exceeds the configured object traversal limit");
        }
        let mut parents = load_snapshot(cas, &hash)?.parent_hashes;
        if parents.len() > max_parents {
            bail!("snapshot has too many parent links");
        }
        parents.sort();
        for parent in parents {
            if !seen.contains(&parent) {
                pending.push_back(parent);
            }
        }
    }
    Ok(false)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileState {
    integrity: String,
    mode: Option<u32>,
}

#[derive(Debug)]
struct CapturedFile {
    state: FileState,
    bytes: Vec<u8>,
}

fn build_manifest(ctx: &SnapshotContext<'_>) -> Result<SourceManifest> {
    let mut files = BTreeMap::new();
    walk_worktree(
        &ctx.project_path,
        &ctx.project_path,
        Path::new(""),
        ctx.state.ignore_matcher.as_ref(),
        None,
        &mut files,
    )?;
    let mut items = HashMap::new();
    for (path, captured) in files {
        let blob_hash = ctx.cas.store_blob(&captured.bytes)?;
        let item = ItemSource {
            item_ref: path.clone(),
            content_blob_hash: blob_hash,
            integrity: captured.state.integrity,
            signature_info: None,
            mode: captured.state.mode,
        };
        items.insert(path, ctx.cas.store_object(&item.to_value())?);
    }
    Ok(SourceManifest {
        item_source_hashes: items,
    })
}

#[cfg(unix)]
fn walk_worktree(
    root: &Path,
    dir: &Path,
    relative_dir: &Path,
    ignore: &crate::ignore::IgnoreMatcher,
    deadline: Option<Instant>,
    files: &mut BTreeMap<String, CapturedFile>,
) -> Result<bool> {
    let directory = open_directory_no_follow(root, dir)?;
    let descriptor_dir = descriptor_path(&directory, dir)?.0;
    let mut entries = fs::read_dir(&descriptor_dir)?.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Ok(false);
        }
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            bail!(
                "project snapshots do not support symlinks: {}",
                entry.path().display()
            );
        }
        let path = entry.path();
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow!("project snapshot path is not valid UTF-8"))?;
        let relative_path = relative_dir.join(name);
        let rel = relative_path
            .to_str()
            .ok_or_else(|| anyhow!("project snapshot path is not valid UTF-8"))?
            .replace('\\', "/");
        if rel == "state" || rel.starts_with("state/") || ignore.is_ignored(&rel) {
            continue;
        }
        if file_type.is_dir() {
            if !walk_worktree(root, &path, &relative_path, ignore, deadline, files)? {
                return Ok(false);
            }
        } else if file_type.is_file() {
            let Some(captured) = capture_regular_file_no_follow(root, &path)? else {
                continue;
            };
            files.insert(rel, captured);
        } else {
            bail!(
                "project snapshots support only regular files and directories: {}",
                path.display()
            );
        }
    }
    Ok(true)
}

#[cfg(not(unix))]
fn walk_worktree(
    _root: &Path,
    dir: &Path,
    _relative_dir: &Path,
    _ignore: &crate::ignore::IgnoreMatcher,
    _deadline: Option<Instant>,
    _files: &mut BTreeMap<String, CapturedFile>,
) -> Result<bool> {
    bail!(
        "secure no-follow project snapshot traversal is unavailable on this platform: {}",
        dir.display()
    )
}

fn manifest_state_map(
    cas: &CasStore,
    items: &HashMap<String, String>,
) -> Result<BTreeMap<String, FileState>> {
    let mut states = BTreeMap::new();
    for (path, hash) in items {
        let item = ItemSource::from_value(&load_verified_object(cas, hash)?)?;
        if item.item_ref != *path {
            anyhow::bail!(
                "manifest path {:?} does not match embedded item_source path {:?}",
                path,
                item.item_ref
            );
        }
        states.insert(
            path.clone(),
            FileState {
                integrity: item.integrity,
                mode: item.mode,
            },
        );
    }
    Ok(states)
}

fn load_snapshot(cas: &CasStore, hash: &str) -> Result<ProjectSnapshot> {
    ProjectSnapshot::from_value(&load_verified_object(cas, hash)?)
}

fn load_snapshot_and_manifest(
    cas: &CasStore,
    hash: &str,
) -> Result<(ProjectSnapshot, SourceManifest)> {
    let snapshot = load_snapshot(cas, hash)?;
    let manifest =
        SourceManifest::from_value(&load_verified_object(cas, &snapshot.project_manifest_hash)?)?;
    Ok((snapshot, manifest))
}

fn load_verified_object(cas: &CasStore, hash: &str) -> Result<Value> {
    ryeos_state::object_closure::load_exact_cas_object_with_cas(
        cas,
        hash,
        ryeos_state::object_closure::ObjectClosureLimits::default().max_object_bytes,
    )
}

fn canonical_project_path(path: &Path) -> Result<PathBuf> {
    let canonical = path
        .canonicalize()
        .with_context(|| format!("canonicalize project path {}", path.display()))?;
    if !canonical.is_dir() {
        bail!("project path is not a directory: {}", canonical.display());
    }
    Ok(canonical)
}

fn acquire_cas_read_guard(ctx: &SnapshotContext<'_>) -> Result<ryeos_state::CasMutationGuard> {
    let guard =
        ryeos_state::CasMutationGuard::acquire_shared(&ctx.state.config.runtime_state_dir())?;
    guard.ensure_protects_cas_root(ctx.cas.root())?;
    Ok(guard)
}

/// Capture one exact regular-file observation. The descriptor is opened with
/// no-follow semantics, checked to resolve beneath the canonical capability
/// root, and supplies both the bytes and mode used by the manifest.
#[cfg(unix)]
fn capture_regular_file_no_follow(root: &Path, path: &Path) -> Result<Option<CapturedFile>> {
    let mut file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "open snapshot input without following links: {}",
                    path.display()
                )
            })
        }
    };
    let before = require_regular_metadata(&file, path)?;
    descriptor_path(&file, path)
        .and_then(|(_, resolved)| ensure_beneath_root(root, path, &resolved))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .with_context(|| format!("read snapshot input {}", path.display()))?;
    let after = require_regular_metadata(&file, path)?;
    if before.dev() != after.dev()
        || before.ino() != after.ino()
        || before.len() != after.len()
        || before.mtime() != after.mtime()
        || before.mtime_nsec() != after.mtime_nsec()
        || before.ctime() != after.ctime()
        || before.ctime_nsec() != after.ctime_nsec()
    {
        bail!(
            "snapshot input changed while it was being captured: {}",
            path.display()
        );
    }
    let raw_mode = after.permissions().mode() & 0o7777;
    let mode = (raw_mode & 0o111 != 0).then_some(raw_mode);
    Ok(Some(CapturedFile {
        state: FileState {
            integrity: sha256_hex(&bytes),
            mode,
        },
        bytes,
    }))
}

#[cfg(unix)]
fn require_regular_metadata(file: &File, path: &Path) -> Result<fs::Metadata> {
    let metadata = file
        .metadata()
        .with_context(|| format!("inspect opened snapshot input {}", path.display()))?;
    if !metadata.file_type().is_file() {
        bail!("snapshot input is not a regular file: {}", path.display());
    }
    Ok(metadata)
}

#[cfg(unix)]
fn open_directory_no_follow(root: &Path, path: &Path) -> Result<File> {
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .with_context(|| {
            format!(
                "open snapshot directory without following links: {}",
                path.display()
            )
        })?;
    let metadata = directory
        .metadata()
        .with_context(|| format!("inspect opened snapshot directory {}", path.display()))?;
    if !metadata.file_type().is_dir() {
        bail!(
            "snapshot traversal entry is not a directory: {}",
            path.display()
        );
    }
    let (_, resolved) = descriptor_path(&directory, path)?;
    ensure_beneath_root(root, path, &resolved)?;
    Ok(directory)
}

#[cfg(unix)]
fn descriptor_path(file: &File, original: &Path) -> Result<(PathBuf, PathBuf)> {
    let fd = file.as_raw_fd();
    for path in [
        PathBuf::from(format!("/proc/self/fd/{fd}")),
        PathBuf::from(format!("/dev/fd/{fd}")),
    ] {
        if let Ok(resolved) = path.canonicalize() {
            return Ok((path, resolved));
        }
    }
    bail!(
        "cannot resolve opened snapshot descriptor for {}",
        original.display()
    )
}

#[cfg(unix)]
fn ensure_beneath_root(root: &Path, original: &Path, resolved: &Path) -> Result<()> {
    if !resolved.starts_with(root) {
        bail!(
            "snapshot input escaped the project capability root: {} resolved to {}",
            original.display(),
            resolved.display()
        );
    }
    Ok(())
}

#[cfg(not(unix))]
fn capture_regular_file_no_follow(_root: &Path, path: &Path) -> Result<Option<CapturedFile>> {
    bail!(
        "secure no-follow project snapshot capture is unavailable on this platform: {}",
        path.display()
    )
}
