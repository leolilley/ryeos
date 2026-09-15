//! Node-authoritative project snapshot reads and mutation.
//!
//! The terminal tool is only a callback client. It never loads the node key,
//! invents a trust store, reads authoritative refs, or publishes HEAD itself.

use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use lillux::cas::CasStore;
use lillux::time::{Duration, MonotonicDeadline, MonotonicTimer};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::callback_token::{CallbackCapability, ThreadAuthState};
use crate::execution_policy::{LIVE_PROJECT_READ_CAPABILITY, LIVE_PROJECT_WRITE_CAPABILITY};
use crate::state::AppState;
use crate::state_store::NodeIdentitySigner;
use ryeos_bundle::manifest::ProjectSnapshotOperation;
use ryeos_bundle::runtime_authority::project_snapshot_cap;
use ryeos_runtime::authorizer::{AuthorizationPolicy, Authorizer};
use ryeos_state::objects::{ProjectFile, ProjectSnapshot, ProjectSnapshotPolicy, ProjectTree};
use ryeos_state::project_sync::ProjectSyncScope;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeProjectSnapshotRequest {
    pub thread_id: String,
    pub operation: ProjectSnapshotOperation,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
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
    ignore_matcher: &'a crate::ignore::IgnoreMatcher,
    project_path: PathBuf,
    project_hash: String,
    principal_key: String,
    authority: ryeos_state::PinnedStateAuthority,
    cas: CasStore,
}

pub struct RuntimeProjectSnapshotService;

fn authorize_snapshot_operation(
    authorizer: &Authorizer,
    project_capability_ceiling: &[String],
    runtime_caps: &[String],
    operation: &ProjectSnapshotOperation,
) -> Result<()> {
    let required_project_capability = match operation {
        ProjectSnapshotOperation::Status
        | ProjectSnapshotOperation::Log
        | ProjectSnapshotOperation::Show => LIVE_PROJECT_READ_CAPABILITY,
        ProjectSnapshotOperation::Create => LIVE_PROJECT_WRITE_CAPABILITY,
    };
    authorizer
        .authorize(
            project_capability_ceiling,
            &AuthorizationPolicy::require(required_project_capability),
        )
        .with_context(|| {
            format!(
                "admitted caller is missing required capability: {required_project_capability} — \
                 project snapshots require sealed live-project authority"
            )
        })?;

    let required_runtime_capability = project_snapshot_cap(operation);
    authorizer
        .authorize(
            runtime_caps,
            &AuthorizationPolicy::require(&required_runtime_capability),
        )
        .with_context(|| {
            format!(
                "runtime is missing required capability: {required_runtime_capability} — \
                 project snapshots are signed manifest-backed runtime authority"
            )
        })?;
    Ok(())
}

fn authorized_snapshot_project_root<'a>(
    cap: &'a CallbackCapability,
    operation: &ProjectSnapshotOperation,
) -> Result<&'a Path> {
    match operation {
        ProjectSnapshotOperation::Status
        | ProjectSnapshotOperation::Log
        | ProjectSnapshotOperation::Show => cap
            .provenance
            .durable_live_read_root()
            .context("project snapshot read requires sealed live-project authority"),
        ProjectSnapshotOperation::Create => cap
            .provenance
            .durable_live_write_root("project")
            .context("project snapshot creation requires sealed live-project write authority"),
    }
}

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
        authorize_snapshot_operation(
            &state.authorizer,
            cap.provenance.project_authority().capability_ceiling(),
            &cap.effective_caps,
            &request.operation,
        )?;
        let authorized_project_path =
            canonical_project_path(authorized_snapshot_project_root(cap, &request.operation)?)?;
        let params: ProjectParams =
            serde_json::from_value(request.params).context("invalid snapshot params")?;
        let project_path = canonical_project_path(
            params
                .project_path
                .as_deref()
                .unwrap_or(authorized_project_path.as_path()),
        )?;
        if project_path != authorized_project_path {
            bail!("snapshot project_path is outside the callback's sealed live-project authority");
        }
        let canonical = project_path
            .to_str()
            .ok_or_else(|| anyhow!("canonical project_path is not valid UTF-8"))?;
        let authority = state.state_store.pinned_state_authority()?;
        let cas = authority.cas_store()?;
        let ctx = SnapshotContext {
            state,
            ignore_matcher: state.ignore_matcher.as_ref(),
            project_hash: ryeos_state::refs::deployed_project_key(canonical),
            principal_key: ryeos_state::refs::principal_storage_key(&thread_auth.acting_principal)?
                .to_owned(),
            authority,
            cas,
            project_path,
        };
        match request.operation {
            ProjectSnapshotOperation::Status => status(&ctx, &params),
            ProjectSnapshotOperation::Log => log(&ctx, params.limit.unwrap_or(20).max(1)),
            ProjectSnapshotOperation::Create => create(&ctx, params.message, params.allow_empty),
            ProjectSnapshotOperation::Show => show(
                &ctx,
                params
                    .snapshot_hash
                    .as_deref()
                    .ok_or_else(|| anyhow!("snapshot_hash is required"))?,
            ),
        }
    }
}

/// Configured-local-operator status using the node's existing state owner.
///
/// The Both-mode service supplies live AppState or its normal
/// read_only_existing standalone equivalent. Never recreate configuration,
/// private identities, policy, or projection state in a terminal Tool merely
/// to make offline status work. Runtime callers continue through execute()
/// and its sealed project plus manifest-backed callback authority.
pub fn local_operator_status(
    state: &AppState,
    caller: &crate::handler_context::HandlerContext,
    project_path: &Path,
    include_unchanged: bool,
    time_budget_ms: u64,
) -> Result<Value> {
    crate::operator_authority::require_local_configured_operator(state, caller)
        .context("snapshot status requires the configured local operator")?;
    // Authenticate before canonicalization or any other path observation.
    if !project_path.is_absolute() {
        bail!("snapshot status project_path must be an absolute path");
    }
    let project_path = canonical_project_path(project_path)?;
    let canonical = project_path
        .to_str()
        .ok_or_else(|| anyhow!("canonical project_path is not valid UTF-8"))?;
    let authority = state.state_store.pinned_state_authority()?;
    let cas = authority.cas_store()?;
    let ctx = SnapshotContext {
        state,
        ignore_matcher: state.ignore_matcher.as_ref(),
        project_hash: ryeos_state::refs::deployed_project_key(canonical),
        principal_key: ryeos_state::refs::principal_storage_key(&caller.fingerprint)?.to_owned(),
        project_path,
        authority,
        cas,
    };
    // Only the comparison is shared. This local-operator entry does not
    // synthesize callback authority or expose snapshot mutation.
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
    ctx.state.state_store.with_state_db(read)
}

fn status(ctx: &SnapshotContext<'_>, params: &ProjectParams) -> Result<Value> {
    let _cas_guard = acquire_cas_read_guard(ctx)?;
    let started = MonotonicTimer::start();
    let deadline = (params.time_budget_ms > 0)
        .then(|| MonotonicDeadline::after(Duration::from_millis(params.time_budget_ms)));
    let (head_hash, deployed_hash) = heads(ctx)?;
    let (head_items, head_policy_hash) = match head_hash.as_deref() {
        Some(hash) => {
            let (snapshot, tree) = load_snapshot_and_manifest(&ctx.cas, hash)?;
            (tree.files, Some(snapshot.effective_policy_hash))
        }
        None => (BTreeMap::new(), None),
    };
    let head_state = manifest_state_map(&ctx.cas, &head_items)?;
    let project_root = lillux::PinnedDirectory::open(&ctx.project_path)?
        .context("snapshot status project root disappeared")?;
    let policy = ryeos_state::project_sync::capture_snapshot_policy_from_pinned(
        &project_root,
        ctx.ignore_matcher,
        ProjectSyncScope::FullProject,
    )?;
    // Status is read-only: derive the same policy identity as create without
    // storing an object, creating a thread, or changing any project ref.
    let policy_hash = ryeos_state::objects::canonical_value_digest(&policy.to_value())?;
    let policy_changed = head_policy_hash
        .as_ref()
        .is_some_and(|head| head != &policy_hash);
    let mut worktree = BTreeMap::new();
    let complete = visit_project_files(&project_root, &policy, deadline, |relative, file| {
        // The traversal already opened this exact regular inode. Keep
        // hashing and portable mode observation at the Lillux boundary;
        // never reopen an ambient path or allocate the whole file here.
        let observed = lillux::observe_open_regular_file(&file)?;
        let (blob_hash, metadata) =
            lillux::digest_open_regular_file_stable_exact(&file, observed.size())?;
        let captured = ProjectFile {
            blob_hash,
            size: metadata.len(),
            normalized_mode: lillux::normalized_portable_regular_mode(&metadata)?,
        };
        worktree.insert(relative.to_owned(), captured.clone());
        Ok(captured)
    })?;
    validate_status_policy_source(&worktree, &policy, complete)?;
    if ryeos_state::project_sync::capture_snapshot_policy_from_pinned(
        &project_root,
        ctx.ignore_matcher,
        ProjectSyncScope::FullProject,
    )? != policy
    {
        bail!("project snapshot policy changed during status inspection");
    }
    project_root.ensure_path_binding()?;
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
        let work = worktree.get(&path);
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
                "worktree_integrity": work.map(|state| state.blob_hash.as_str()),
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
        "effective_policy_hash": policy_hash,
        "head_effective_policy_hash": head_policy_hash,
        "policy_changed": policy_changed,
        "dirty": policy_changed || counts["added"] > 0 || counts["modified"] > 0 || counts["deleted"] > 0,
        "scan_complete": complete,
        "scan_elapsed_ms": started.elapsed_millis(),
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
    let state = ctx.state;
    let guard = ctx.authority.acquire_shared_guard()?;
    let _permit = state
        .write_barrier
        .acquire_with_timeout(crate::write_barrier::ONLINE_WRITE_PERMIT_TIMEOUT)
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
    let locked_head = state
        .state_store
        .with_state_db(|db| db.read_project_head(&ctx.principal_key, &ctx.project_hash))?;
    if locked_head != initial_head {
        bail!("project head changed while creating snapshot; rerun snapshot create");
    }
    match initial_head.as_deref() {
        Some(current) => state.state_store.advance_project_head_ref(
            &ctx.principal_key,
            &ctx.project_hash,
            &snapshot_hash,
            current,
            &signer,
            &guard,
        ),
        None => state.state_store.write_project_head_ref(
            &ctx.principal_key,
            &ctx.project_hash,
            &snapshot_hash,
            &signer,
            &guard,
        ),
    }?;
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

/// A soft status deadline stops the existing bounded traversal between entries.
/// It is not a filesystem error, and must never turn an incomplete scan into
/// evidence of deletion. File reads already in progress may finish first.
#[derive(Debug, thiserror::Error)]
#[error("snapshot status scan budget elapsed")]
struct StatusScanBudgetElapsed;

/// Creation and preview have one path/policy/traversal contract. Do not add a
/// status-only ignore table or raw filesystem walker here: the node matcher and
/// project exclusions compose in project_sync; Lillux owns descriptor opening.
/// The visitor receives only already-open byte transport, never path authority.
fn visit_project_files<V>(
    root: &lillux::PinnedDirectory,
    policy: &ProjectSnapshotPolicy,
    deadline: Option<MonotonicDeadline>,
    mut visit: V,
) -> Result<bool>
where
    V: FnMut(&str, std::fs::File) -> Result<ProjectFile>,
{
    let matcher = policy.matcher()?;
    let mut file_count = 0_usize;
    let mut descriptor_bytes = 0_u64;
    let traversal = root.visit_regular_files_bounded(
        lillux::DirectoryTraversalBudget::new(
            ryeos_state::project_sync::MAX_PROJECT_TREE_ENTRIES,
            ryeos_state::project_sync::MAX_PROJECT_TREE_DEPTH,
        ),
        |relative, _is_directory| {
            if deadline.is_some_and(|deadline| deadline.has_elapsed()) {
                return Err(StatusScanBudgetElapsed.into());
            }
            let rel = canonical_relative_path(relative)?;
            Ok(
                ryeos_state::project_sync::is_project_snapshot_floor_excluded(&rel)
                    || matcher.is_ignored(&rel),
            )
        },
        |relative, file| {
            if file_count >= ryeos_state::project_sync::MAX_PROJECT_TREE_FILES {
                bail!(
                    "project snapshot exceeds {} regular files",
                    ryeos_state::project_sync::MAX_PROJECT_TREE_FILES
                );
            }
            file_count += 1;
            let rel = canonical_relative_path(relative)?;
            ryeos_state::project_sync::validate_project_manifest_path(
                &rel,
                policy.sync_scope,
                Some(&matcher),
            )?;
            let project_file = visit(&rel, file)?;
            project_file.validate()?;
            let object_bytes = lillux::canonical_json(&project_file.to_value())?.len() as u64;
            descriptor_bytes = descriptor_bytes
                .checked_add(object_bytes)
                .and_then(|total| total.checked_add(rel.len() as u64))
                .ok_or_else(|| anyhow!("project snapshot descriptor byte count overflow"))?;
            if descriptor_bytes
                > ryeos_state::project_materialization::MAX_PROJECT_TREE_DESCRIPTOR_BYTES
            {
                bail!(
                    "project snapshot exceeds {} descriptor bytes",
                    ryeos_state::project_materialization::MAX_PROJECT_TREE_DESCRIPTOR_BYTES
                );
            }
            Ok(())
        },
    );
    match traversal {
        Ok(()) => Ok(true),
        Err(error) if error.is::<StatusScanBudgetElapsed>() => Ok(false),
        Err(error) => Err(error),
    }
}

fn build_project_tree(
    ctx: &SnapshotContext<'_>,
    project_root: &lillux::PinnedDirectory,
    policy: &ProjectSnapshotPolicy,
) -> Result<ProjectTree> {
    let mut files = BTreeMap::new();
    visit_project_files(project_root, policy, None, |relative, file| {
        let blob = ctx
            .cas
            .put_blob_from_open_regular(file, &project_root.path().join(relative))?;
        let project_file = ProjectFile {
            blob_hash: blob.hash,
            size: blob.size,
            normalized_mode: blob.normalized_mode,
        };
        project_file.validate()?;
        let file_hash = ctx.cas.store_object(&project_file.to_value())?;
        if files.insert(relative.to_owned(), file_hash).is_some() {
            bail!("duplicate canonical project path during snapshot: {relative}");
        }
        Ok(project_file)
    })?;
    let tree = ProjectTree { files };
    ryeos_state::project_sync::validate_project_tree_paths(&tree, policy)?;
    Ok(tree)
}

fn validate_status_policy_source(
    files: &BTreeMap<String, ProjectFile>,
    policy: &ProjectSnapshotPolicy,
    complete: bool,
) -> Result<()> {
    let source_path = ryeos_state::project_sync::PROJECT_SNAPSHOT_CONFIG_RELATIVE;
    let observed = files.get(source_path);
    if !complete && observed.is_none() {
        // An unvisited policy source is not evidence of absence. Its current
        // descriptor-rooted policy is still rechecked before status returns.
        return Ok(());
    }
    let mut tree = ProjectTree {
        files: BTreeMap::new(),
    };
    if let Some(file) = observed {
        tree.files.insert(
            source_path.to_owned(),
            ryeos_state::objects::canonical_value_digest(&file.to_value())?,
        );
    }
    ryeos_state::project_sync::validate_captured_policy_source_from_files(&tree, policy, files)
}

fn canonical_relative_path(path: &Path) -> Result<String> {
    let rel = path
        .to_str()
        .ok_or_else(|| anyhow!("project snapshot path is not valid UTF-8"))?
        .replace('\\', "/");
    ryeos_state::project_sync::validate_safe_relative_path(&rel)?;
    Ok(rel)
}

fn manifest_state_map(
    cas: &CasStore,
    items: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, ProjectFile>> {
    let mut states = BTreeMap::new();
    for (path, hash) in items {
        let item = ProjectFile::from_value(&load_verified_object(cas, hash)?)?;
        states.insert(path.clone(), item);
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
    let canonical = lillux::canonicalize_existing_path(path)
        .with_context(|| format!("canonicalize project path {}", path.display()))?;
    let root = lillux::PinnedDirectory::open(&canonical)?
        .with_context(|| format!("project directory disappeared: {}", canonical.display()))?;
    root.ensure_path_binding()?;
    Ok(canonical)
}

fn acquire_cas_read_guard(ctx: &SnapshotContext<'_>) -> Result<ryeos_state::CasMutationGuard> {
    ctx.authority.acquire_shared_guard()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;

    #[test]
    fn snapshot_callback_requires_project_and_manifest_runtime_authority() {
        let authorizer = Authorizer::new();
        let project_ceiling = vec![LIVE_PROJECT_WRITE_CAPABILITY.to_string()];
        let create_runtime = vec![project_snapshot_cap(&ProjectSnapshotOperation::Create)];
        assert!(
            authorize_snapshot_operation(
                &authorizer,
                &project_ceiling,
                &create_runtime,
                &ProjectSnapshotOperation::Create,
            )
            .is_ok()
        );

        let missing_project = authorize_snapshot_operation(
            &authorizer,
            &[],
            &create_runtime,
            &ProjectSnapshotOperation::Create,
        )
        .unwrap_err()
        .to_string();
        assert!(
            missing_project.contains(LIVE_PROJECT_WRITE_CAPABILITY),
            "got: {missing_project}"
        );

        let missing_runtime = authorize_snapshot_operation(
            &authorizer,
            &project_ceiling,
            &[],
            &ProjectSnapshotOperation::Create,
        )
        .unwrap_err()
        .to_string();
        assert!(
            missing_runtime.contains("ryeos.create.project-snapshots.live"),
            "got: {missing_runtime}"
        );
    }

    #[test]
    fn snapshot_status_callback_keeps_both_existing_authority_requirements() {
        let authorizer = Authorizer::new();
        let read_project = vec![LIVE_PROJECT_READ_CAPABILITY.to_owned()];
        let status_runtime = vec![project_snapshot_cap(&ProjectSnapshotOperation::Status)];
        assert!(
            authorize_snapshot_operation(
                &authorizer,
                &read_project,
                &status_runtime,
                &ProjectSnapshotOperation::Status,
            )
            .is_ok()
        );
        for (project, runtime) in [
            (Vec::new(), status_runtime.clone()),
            (read_project.clone(), Vec::new()),
        ] {
            assert!(
                authorize_snapshot_operation(
                    &authorizer,
                    &project,
                    &runtime,
                    &ProjectSnapshotOperation::Status,
                )
                .is_err()
            );
        }
    }

    #[test]
    fn snapshot_callback_runtime_authority_is_operation_specific() {
        let authorizer = Authorizer::new();
        let project_ceiling = vec![LIVE_PROJECT_WRITE_CAPABILITY.to_string()];
        let status_runtime = vec![project_snapshot_cap(&ProjectSnapshotOperation::Status)];
        let error = authorize_snapshot_operation(
            &authorizer,
            &project_ceiling,
            &status_runtime,
            &ProjectSnapshotOperation::Create,
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("ryeos.create.project-snapshots.live"),
            "got: {error}"
        );
    }

    #[test]
    fn snapshot_callback_project_authority_is_operation_specific() {
        let authorizer = Authorizer::new();
        let write_only_ceiling = vec![LIVE_PROJECT_WRITE_CAPABILITY.to_string()];
        let status_runtime = vec![project_snapshot_cap(&ProjectSnapshotOperation::Status)];
        let error = authorize_snapshot_operation(
            &authorizer,
            &write_only_ceiling,
            &status_runtime,
            &ProjectSnapshotOperation::Status,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains(LIVE_PROJECT_READ_CAPABILITY), "got: {error}");
    }

    #[test]
    fn snapshot_callback_params_reject_unknown_and_removed_fields() {
        let error = serde_json::from_value::<ProjectParams>(serde_json::json!({
            "project": "/project"
        }))
        .unwrap_err()
        .to_string();
        assert!(error.contains("unknown field `project`"), "got: {error}");
    }

    #[test]
    fn snapshot_walk_omits_symlinks_without_following_them() {
        let project = tempfile::tempdir().unwrap();
        fs::write(project.path().join("regular.txt"), b"regular").unwrap();
        let bin = project.path().join(".venv/bin");
        fs::create_dir_all(&bin).unwrap();
        symlink("/usr/bin/python", bin.join("python")).unwrap();

        let mut files = BTreeMap::new();
        let root = lillux::PinnedDirectory::open(project.path())
            .unwrap()
            .unwrap();
        let policy = ryeos_state::project_sync::capture_snapshot_policy_from_pinned(
            &root,
            &crate::ignore::IgnoreMatcher::from_config(&crate::ignore::IgnoreConfig {
                patterns: vec![".venv/".to_owned()],
            })
            .unwrap(),
            ProjectSyncScope::FullProject,
        )
        .unwrap();
        let complete = visit_project_files(&root, &policy, None, |relative, file| {
            let observation = lillux::observe_open_regular_file(&file)?;
            let (blob_hash, metadata) =
                lillux::digest_open_regular_file_stable_exact(&file, observation.size())?;
            let captured = ProjectFile {
                blob_hash,
                size: metadata.len(),
                normalized_mode: lillux::normalized_portable_regular_mode(&metadata)?,
            };
            files.insert(relative.to_owned(), captured.clone());
            Ok(captured)
        })
        .unwrap();

        assert!(complete);
        assert!(files.contains_key("regular.txt"));
        assert!(!files.contains_key(".venv/bin/python"));
    }

    #[test]
    fn status_deadline_stops_shared_traversal_without_claiming_completeness() {
        let project = tempfile::tempdir().unwrap();
        fs::write(project.path().join("file"), b"data").unwrap();
        let root = lillux::PinnedDirectory::open(project.path())
            .unwrap()
            .unwrap();
        let matcher = crate::ignore::IgnoreMatcher::from_config(&crate::ignore::IgnoreConfig {
            patterns: Vec::new(),
        })
        .unwrap();
        let policy = ryeos_state::project_sync::capture_snapshot_policy_from_pinned(
            &root,
            &matcher,
            ProjectSyncScope::FullProject,
        )
        .unwrap();
        assert!(
            !visit_project_files(
                &root,
                &policy,
                Some(MonotonicDeadline::after(Duration::ZERO)),
                |_, _| panic!("expired scan must not observe file bodies"),
            )
            .unwrap()
        );
        symlink("file", project.path().join("link")).unwrap();
        let error = visit_project_files(&root, &policy, None, |_, _| {
            Ok(ProjectFile {
                blob_hash: "a".repeat(64),
                size: 4,
                normalized_mode: ProjectFile::REGULAR_MODE,
            })
        })
        .unwrap_err();
        assert!(!error.is::<StatusScanBudgetElapsed>(), "{error:#}");
    }

    #[test]
    fn status_policy_source_uses_capture_proof_even_for_partial_observations() {
        let project = tempfile::tempdir().unwrap();
        let source = ryeos_state::project_sync::PROJECT_SNAPSHOT_CONFIG_RELATIVE;
        let source_path = project.path().join(source);
        fs::create_dir_all(source_path.parent().unwrap()).unwrap();
        let bytes = b"schema: 1\nexclusions: []\n";
        fs::write(&source_path, bytes).unwrap();
        let matcher = crate::ignore::IgnoreMatcher::from_config(&crate::ignore::IgnoreConfig {
            patterns: Vec::new(),
        })
        .unwrap();
        let policy = ryeos_state::project_sync::capture_snapshot_policy(
            project.path(),
            &matcher,
            ProjectSyncScope::FullProject,
        )
        .unwrap();
        let mut files = BTreeMap::new();
        assert!(validate_status_policy_source(&files, &policy, false).is_ok());
        assert!(validate_status_policy_source(&files, &policy, true).is_err());
        files.insert(
            source.to_owned(),
            ProjectFile {
                blob_hash: lillux::sha256_hex(bytes),
                size: bytes.len() as u64,
                normalized_mode: ProjectFile::REGULAR_MODE,
            },
        );
        assert!(validate_status_policy_source(&files, &policy, false).is_ok());
        assert!(validate_status_policy_source(&files, &policy, true).is_ok());
        files.get_mut(source).unwrap().blob_hash = "a".repeat(64);
        assert!(validate_status_policy_source(&files, &policy, false).is_err());
        assert!(validate_status_policy_source(&files, &policy, true).is_err());
    }
}
