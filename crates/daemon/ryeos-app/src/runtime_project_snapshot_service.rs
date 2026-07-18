//! Daemon-authoritative project snapshot reads and mutation.
//!
//! The terminal tool is only a callback client. It never loads the node key,
//! invents a trust store, reads authoritative refs, or publishes HEAD itself.

use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};
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
use ryeos_state::objects::{ProjectFile, ProjectSnapshot, ProjectSnapshotPolicy, ProjectTree};
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
    state: SnapshotState<'a>,
    ignore_matcher: &'a crate::ignore::IgnoreMatcher,
    project_path: PathBuf,
    project_hash: String,
    principal_key: String,
    authority: ryeos_state::PinnedStateAuthority,
    cas: CasStore,
}

enum SnapshotState<'a> {
    Live(&'a AppState),
    Offline(&'a ryeos_state::StateDb),
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
        let authority = state
            .state_store
            .with_state_db(|db| db.pinned_authority())?;
        let cas = authority.cas_store()?;
        let ctx = SnapshotContext {
            state: SnapshotState::Live(state),
            ignore_matcher: state.ignore_matcher.as_ref(),
            project_hash: ryeos_state::refs::deployed_project_key(canonical),
            principal_key: ryeos_state::refs::principal_storage_key(&thread_auth.acting_principal)?
                .to_owned(),
            authority,
            cas,
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

/// Read snapshot status without a daemon thread. This uses the fail-only,
/// non-repairing projection opener and the same shared CAS guard as the live
/// service, preserving signed-head verification without requiring callback
/// authority for a read-only command.
pub fn offline_status(
    app_root: &Path,
    project_path: &Path,
    include_unchanged: bool,
    time_budget_ms: u64,
) -> Result<Value> {
    let config = crate::config::Config::load(&crate::config::ConfigSources {
        app_root: Some(app_root.to_path_buf()),
        ..crate::config::ConfigSources::default()
    })
    .context("load local node configuration for snapshot status")?;
    let identity = crate::identity::NodeIdentity::load(&config.node_signing_key_path)
        .context("load node identity for signed-head verification")?;
    let mut head_trust = ryeos_state::refs::TrustStore::new();
    head_trust.insert(
        identity.fingerprint().to_string(),
        *identity.verifying_key(),
    );
    let state_db = ryeos_state::StateDb::open_for_projection_verification(
        &config.runtime_state_dir(),
        std::sync::Arc::new(head_trust),
    )
    .context("open local snapshot projection for verification")?;
    let authority = state_db.pinned_authority()?;
    let cas = authority.cas_store()?;
    let operator_key = lillux::crypto::load_signing_key(&config.operator_signing_key_path)
        .context("load operator identity for principal snapshot head")?;
    let principal = format!(
        "fp:{}",
        lillux::signature::compute_fingerprint(&operator_key.verifying_key())
    );
    let project_path = canonical_project_path(project_path)?;
    let canonical = project_path
        .to_str()
        .ok_or_else(|| anyhow!("canonical project_path is not valid UTF-8"))?;
    let project_hash = ryeos_state::refs::deployed_project_key(canonical);
    let ignore_matcher = crate::ignore::load_from_app_root(&config.app_root)?;
    let ctx = SnapshotContext {
        state: SnapshotState::Offline(&state_db),
        ignore_matcher: &ignore_matcher,
        project_path,
        project_hash,
        principal_key: ryeos_state::refs::principal_storage_key(&principal)?.to_owned(),
        authority,
        cas,
    };
    status(
        &ctx,
        &ProjectParams {
            project_path: None,
            include_unchanged,
            time_budget_ms,
            limit: None,
            message: None,
            allow_empty: false,
            snapshot_hash: None,
        },
    )
}

fn heads(ctx: &SnapshotContext<'_>) -> Result<(Option<String>, Option<String>)> {
    let read = |db: &ryeos_state::StateDb| {
        Ok((
            db.read_project_head(&ctx.principal_key, &ctx.project_hash)?,
            db.read_deployed_project_ref(&ctx.project_hash)?
                .map(|head| head.target_hash),
        ))
    };
    match ctx.state {
        SnapshotState::Live(state) => state.state_store.with_state_db(read),
        SnapshotState::Offline(db) => read(db),
    }
}

fn status(ctx: &SnapshotContext<'_>, params: &ProjectParams) -> Result<Value> {
    let _cas_guard = acquire_cas_read_guard(ctx)?;
    let started = Instant::now();
    let deadline =
        (params.time_budget_ms > 0).then(|| started + Duration::from_millis(params.time_budget_ms));
    let (head_hash, deployed_hash) = heads(ctx)?;
    let head_items = match head_hash.as_deref() {
        Some(hash) => load_snapshot_and_manifest(&ctx.cas, hash)?.1.files,
        None => BTreeMap::new(),
    };
    let head_state = manifest_state_map(&ctx.cas, &head_items)?;
    let mut worktree = BTreeMap::new();
    // Status compares only snapshot-representable files. Symlinks and special
    // entries fail loudly and are never followed.
    let complete = walk_worktree(
        &ctx.project_path,
        &ctx.project_path,
        Path::new(""),
        ctx.ignore_matcher,
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
                "head_project_file_hash": head_items.get(&path),
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
    let traversal_limits =
        ryeos_state::object_closure::ObjectClosureLimits::for_project_snapshot_transport();
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
            "project_tree_hash": snapshot.project_tree_hash,
            "effective_policy_hash": snapshot.effective_policy_hash,
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
    let SnapshotState::Live(state) = ctx.state else {
        bail!("snapshot creation requires live daemon authority");
    };
    let guard = ctx.authority.acquire_shared_guard()?;
    let _permit = state
        .write_barrier
        .try_acquire()
        .map_err(|error| anyhow!("cannot acquire snapshot write permit: {error}"))?;
    let (initial_head, _) = heads(ctx)?;
    let current = initial_head
        .as_deref()
        .map(|hash| load_snapshot(&ctx.cas, hash))
        .transpose()?;
    let project_root = lillux::PinnedDirectory::open(&ctx.project_path)?.ok_or_else(|| {
        anyhow!(
            "project root does not exist: {}",
            ctx.project_path.display()
        )
    })?;
    let policy = ryeos_state::project_sync::capture_snapshot_policy_from_pinned(
        &project_root,
        ctx.ignore_matcher,
        ProjectSyncScope::FullProject,
    )?;
    let policy_hash = ctx.cas.store_object(&policy.to_value())?;
    let tree = build_project_tree(ctx, &project_root, &policy)?;
    ryeos_state::project_sync::validate_captured_policy_source(&ctx.cas, &tree, &policy)?;
    let policy_after = ryeos_state::project_sync::capture_snapshot_policy_from_pinned(
        &project_root,
        ctx.ignore_matcher,
        ProjectSyncScope::FullProject,
    )?;
    if policy_after != policy {
        bail!("project snapshot policy changed during capture");
    }
    project_root.ensure_path_binding()?;
    let tree_hash = ctx.cas.store_object(&tree.to_value())?;
    if !allow_empty
        && current.as_ref().is_some_and(|snapshot| {
            snapshot.project_tree_hash == tree_hash && snapshot.effective_policy_hash == policy_hash
        })
    {
        return Ok(json!({
            "kind": "snapshot_create",
            "project_path": ctx.project_path,
            "project_hash": ctx.project_hash,
            "principal_key": ctx.principal_key,
            "created": false,
            "reason": "clean",
            "head_snapshot_hash": initial_head,
            "tree_hash": tree_hash,
            "tree_entries": tree.files.len(),
            "message": message,
        }));
    }
    let parent_hashes = initial_head.iter().cloned().collect::<Vec<_>>();
    let snapshot = ProjectSnapshot {
        project_tree_hash: tree_hash.clone(),
        effective_policy_hash: policy_hash.clone(),
        message: message.clone(),
        parent_hashes: parent_hashes.clone(),
        created_at: lillux::time::iso8601_now(),
        source: "snapshot_create".to_string(),
    };
    let snapshot_hash = ctx.cas.store_object(&snapshot.to_value())?;
    let signer = NodeIdentitySigner::from_identity(&state.identity);
    state.state_store.with_state_db(|db| {
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
                &guard,
            ),
            None => db.write_project_head_ref(
                &ctx.principal_key,
                &ctx.project_hash,
                &snapshot_hash,
                &signer,
                &guard,
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
        "tree_hash": tree_hash,
        "effective_policy_hash": policy_hash,
        "tree_entries": tree.files.len(),
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
    let (snapshot, tree) = load_snapshot_and_manifest(&ctx.cas, snapshot_hash)?;
    Ok(json!({
        "kind": "snapshot_show",
        "snapshot_hash": snapshot_hash,
        "created_at": snapshot.created_at,
        "source": snapshot.source,
        "message": snapshot.message,
        "project_tree_hash": snapshot.project_tree_hash,
        "effective_policy_hash": snapshot.effective_policy_hash,
        "parent_hashes": snapshot.parent_hashes,
        "tree_entries": tree.files.len(),
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
    mode: u32,
    size: u64,
}

#[derive(Debug)]
struct CapturedFile {
    state: FileState,
    bytes: Vec<u8>,
}

fn build_project_tree(
    ctx: &SnapshotContext<'_>,
    project_root: &lillux::PinnedDirectory,
    policy: &ProjectSnapshotPolicy,
) -> Result<ProjectTree> {
    let matcher = policy.matcher()?;
    let mut files = BTreeMap::new();
    project_root.visit_regular_files(
        |relative, _is_directory| {
            let rel = canonical_relative_path(relative)?;
            Ok(
                ryeos_state::project_sync::is_project_snapshot_floor_excluded(&rel)
                    || matcher.is_ignored(&rel),
            )
        },
        |relative, file| {
            let rel = canonical_relative_path(relative)?;
            ryeos_state::project_sync::validate_project_manifest_path(
                &rel,
                policy.sync_scope,
                Some(&matcher),
            )?;
            let blob = ctx
                .cas
                .put_blob_from_open_regular(file, &project_root.path().join(relative))?;
            let project_file = ProjectFile {
                blob_hash: blob.hash,
                size: blob.size,
                normalized_mode: blob.normalized_mode,
            };
            let file_hash = ctx.cas.store_object(&project_file.to_value())?;
            files.insert(rel, file_hash);
            Ok(())
        },
    )?;
    let tree = ProjectTree { files };
    ryeos_state::project_sync::validate_project_tree_paths(&tree, policy)?;
    Ok(tree)
}

fn canonical_relative_path(path: &Path) -> Result<String> {
    let rel = path
        .to_str()
        .ok_or_else(|| anyhow!("project snapshot path is not valid UTF-8"))?
        .replace('\\', "/");
    ryeos_state::project_sync::validate_safe_relative_path(&rel)?;
    Ok(rel)
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
    items: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, FileState>> {
    let mut states = BTreeMap::new();
    for (path, hash) in items {
        let item = ProjectFile::from_value(&load_verified_object(cas, hash)?)?;
        states.insert(
            path.clone(),
            FileState {
                integrity: item.blob_hash,
                mode: item.normalized_mode,
                size: item.size,
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
) -> Result<(ProjectSnapshot, ProjectTree)> {
    let snapshot = load_snapshot(cas, hash)?;
    let tree = ProjectTree::from_value(&load_verified_object(cas, &snapshot.project_tree_hash)?)?;
    let policy = ProjectSnapshotPolicy::from_value(&load_verified_object(
        cas,
        &snapshot.effective_policy_hash,
    )?)?;
    ryeos_state::project_sync::validate_project_tree_paths(&tree, &policy)?;
    ryeos_state::project_sync::validate_captured_policy_source(cas, &tree, &policy)?;
    Ok((snapshot, tree))
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
    ctx.authority.acquire_shared_guard()
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
    let mode = ProjectFile::normalize_mode(raw_mode);
    Ok(Some(CapturedFile {
        state: FileState {
            integrity: sha256_hex(&bytes),
            mode,
            size: bytes.len() as u64,
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn snapshot_walk_omits_symlinks_without_following_them() {
        let project = tempfile::tempdir().unwrap();
        fs::write(project.path().join("regular.txt"), b"regular").unwrap();
        let bin = project.path().join(".venv/bin");
        fs::create_dir_all(&bin).unwrap();
        symlink("/usr/bin/python", bin.join("python")).unwrap();

        let mut files = BTreeMap::new();
        let complete = walk_worktree(
            project.path(),
            project.path(),
            Path::new(""),
            &crate::ignore::matcher_from_builtins(),
            None,
            &mut files,
        )
        .unwrap();

        assert!(complete);
        assert!(files.contains_key("regular.txt"));
        assert!(!files.contains_key(".venv/bin/python"));
    }
}
