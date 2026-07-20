//! Execution lifecycle: checkout, execute, fold-back.
//!
//! Manages the CAS-backed execution flow:
//! 1. Checkout project from CAS to working directory
//! 2. After execution, diff working dir and fold back changes

pub mod arch_check;
pub mod cache;
pub mod ingest;
pub mod launch;
pub(crate) mod launch_claim;
pub mod launch_envelope;
pub mod launch_preparation;
pub mod lillux_bridge;
pub mod limits;
pub(crate) mod process_attachment;
pub mod project_source;
pub mod runner;
pub mod runtime_dispatch;
pub mod spawn_detached_child;
pub mod spawn_follow_child;
pub mod thread_meta;
pub mod workspace;

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::path::{Component, Path, PathBuf};

use anyhow::Result;
use ryeos_app::runtime_db::WorkspaceState;

use ryeos_state::objects::{ProjectSnapshotPolicy, ProjectTree};
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

/// A result snapshot whose newly-written closure remains a durable temporary
/// GC root until the caller binds the snapshot into authoritative thread/head
/// state. Dropping it abandons a conservative recovery root; it never creates
/// an unrooted publication window.
pub(crate) struct PendingProjectResult {
    pub(crate) snapshot_hash: String,
    pub(crate) publication: Option<PendingCasPublication>,
    pub(crate) quiesced: Option<QuiescedExecutionGroup>,
}

impl PendingProjectResult {
    pub(crate) fn snapshot_hash(&self) -> &str {
        &self.snapshot_hash
    }

    pub(crate) fn publish(mut self) -> Result<()> {
        if let Some(publication) = self.publication.take() {
            publication.publish()?;
        }
        self.quiesced.take();
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

/// Internal one-traversal tree capture. It is never launch authority by itself;
/// only promotion to [`CapturedProjectGeneration`] may cross thread birth.
pub(crate) struct StagedProjectTree {
    pub hash: String,
    pub policy_hash: String,
    publication: PendingCasPublication,
}

/// Move-only immutable execution authority shared by admission, birth,
/// materialization, continuation and recovery.
pub struct CapturedProjectGeneration {
    pub(crate) snapshot_hash: String,
    pub(crate) tree_hash: String,
    pub(crate) policy_hash: String,
    pub(crate) stable_project_identity: ryeos_app::launch_metadata::StableProjectIdentity,
    pub(crate) local_overlay_root: Option<PathBuf>,
    publication: PendingCasPublication,
}

impl CapturedProjectGeneration {
    pub fn snapshot_hash(&self) -> &str {
        &self.snapshot_hash
    }

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
    origin_site: &str,
    source: &str,
) -> Result<CapturedProjectGeneration> {
    let pending = capture_live_project_tree(state, project_path, source)?;
    capture_tree_project_snapshot(
        state,
        pending.hash,
        pending.policy_hash,
        ryeos_app::launch_metadata::StableProjectIdentity::from_path(project_path, origin_site)?,
        Some(project_path.to_path_buf()),
        source,
        pending.publication,
    )
}

pub(crate) fn derive_pinned_child_authority(
    parent: &ryeos_state::objects::ExecutionProjectAuthority,
    snapshot_hash: String,
    realization: ryeos_state::objects::PinnedChildProjectRealization,
    capability_ceiling: &[String],
) -> Result<ryeos_state::objects::ExecutionProjectAuthority> {
    let (stable_identity, display_path, environment) = match parent {
        ryeos_state::objects::ExecutionProjectAuthority::LiveProject {
            authored_project_identity,
            canonical_root,
            environment,
            ..
        } => (
            authored_project_identity.clone(),
            Some(canonical_root.clone()),
            environment.clone(),
        ),
        ryeos_state::objects::ExecutionProjectAuthority::PinnedGeneration {
            stable_project_identity,
            display_path,
            environment,
            ..
        } => (
            stable_project_identity.clone(),
            display_path.clone(),
            environment.clone(),
        ),
        ryeos_state::objects::ExecutionProjectAuthority::Projectless { .. } => {
            anyhow::bail!("pin-at-spawn requires project-backed parent authority")
        }
    };
    ryeos_state::objects::ExecutionProjectAuthority::pinned(
        stable_identity,
        display_path,
        snapshot_hash,
        match realization {
            ryeos_state::objects::PinnedChildProjectRealization::ReadOnly => {
                ryeos_state::objects::PinnedProjectRealization::ReadOnly
            }
            ryeos_state::objects::PinnedChildProjectRealization::CowDiscard => {
                ryeos_state::objects::PinnedProjectRealization::Cow {
                    terminal_publication: ryeos_state::objects::PinnedTerminalPublication::Discard,
                }
            }
        },
        environment,
        capability_ceiling.to_vec(),
    )?
    .with_child_policy(ryeos_state::objects::ChildProjectAuthorityPolicy::Inherit)
}

/// Capture a live project tree under a durable recovery root. The
/// shared guard is acquired from the same descriptor-pinned authority before
/// the first blob write and remains held until the staged root is durable.
pub(crate) fn capture_live_project_tree(
    state: &ryeos_app::state::AppState,
    project_path: &Path,
    source: &str,
) -> Result<StagedProjectTree> {
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
    let project_root = lillux::PinnedDirectory::open(project_path)?.ok_or_else(|| {
        anyhow::anyhow!(
            "cannot snapshot missing project directory {}",
            project_path.display()
        )
    })?;
    let policy = ryeos_state::project_sync::capture_snapshot_policy_from_pinned(
        &project_root,
        &state.ignore_matcher,
        ryeos_state::project_sync::ProjectSyncScope::FullProject,
    )?;
    let policy_hash = staged_roots.store_object_admitted(&guard, &cas, &policy.to_value())?;
    let tree = ingest::ingest_project_tree(&authority, &guard, &project_root, &policy)?;
    ryeos_state::project_sync::validate_captured_policy_source(&cas, &tree, &policy)?;
    let policy_after = ryeos_state::project_sync::capture_snapshot_policy_from_pinned(
        &project_root,
        &state.ignore_matcher,
        ryeos_state::project_sync::ProjectSyncScope::FullProject,
    )?;
    if policy_after != policy {
        anyhow::bail!("project snapshot policy changed during project capture");
    }
    project_root.ensure_path_binding()?;
    let hash = staged_roots.store_object_admitted(&guard, &cas, &tree.to_value())?;
    Ok(StagedProjectTree {
        hash,
        policy_hash,
        publication: PendingCasPublication {
            authority,
            staged_roots: Some(staged_roots),
        },
    })
}

/// Promote an already-staged project tree to a project snapshot under the
/// same pinned runtime/CAS/recovery authority and durable recovery lease.
pub(crate) fn capture_tree_project_snapshot(
    state: &ryeos_app::state::AppState,
    tree_hash: String,
    policy_hash: String,
    stable_project_identity: ryeos_app::launch_metadata::StableProjectIdentity,
    local_overlay_root: Option<PathBuf>,
    source: &str,
    mut publication: PendingCasPublication,
) -> Result<CapturedProjectGeneration> {
    let guard = publication.authority.acquire_shared_guard()?;
    publication.authority.ensure_guard(&guard)?;
    let _permit = state
        .write_barrier
        .try_acquire()
        .map_err(|error| anyhow::anyhow!("cannot acquire CAS write permit: {error}"))?;
    let cas = publication.authority.cas_store()?;
    let tree_value = cas
        .get_object(&tree_hash)?
        .ok_or_else(|| anyhow::anyhow!("project tree {tree_hash} is no longer present in CAS"))?;
    let tree = ProjectTree::from_value(&tree_value)?;
    let policy_value = cas.get_object(&policy_hash)?.ok_or_else(|| {
        anyhow::anyhow!("project snapshot policy {policy_hash} is no longer present in CAS")
    })?;
    let policy = ProjectSnapshotPolicy::from_value(&policy_value)?;
    ryeos_state::project_sync::validate_project_tree_paths(&tree, &policy)?;
    ryeos_state::project_sync::validate_captured_policy_source(&cas, &tree, &policy)?;
    publication
        .staged_roots
        .as_mut()
        .expect("pending CAS publication always owns staged roots")
        .protect_object_hash_admitted(&guard, &tree_hash)?;
    publication
        .staged_roots
        .as_mut()
        .expect("pending CAS publication always owns staged roots")
        .protect_object_hash_admitted(&guard, &policy_hash)?;
    let hash = store_project_snapshot(
        publication
            .staged_roots
            .as_mut()
            .expect("pending CAS publication always owns staged roots"),
        &guard,
        &cas,
        tree_hash.clone(),
        policy_hash.clone(),
        source,
    )?;
    Ok(CapturedProjectGeneration {
        snapshot_hash: hash,
        tree_hash,
        policy_hash,
        stable_project_identity,
        local_overlay_root,
        publication,
    })
}

fn store_project_snapshot(
    staged_roots: &mut ryeos_state::StagedCasRootLease,
    guard: &ryeos_state::CasMutationGuard,
    cas: &lillux::cas::CasStore,
    tree_hash: String,
    policy_hash: String,
    source: &str,
) -> Result<String> {
    let snapshot = ryeos_state::objects::ProjectSnapshot {
        project_tree_hash: tree_hash,
        effective_policy_hash: policy_hash,
        message: None,
        parent_hashes: Vec::new(),
        created_at: lillux::time::iso8601_now(),
        source: source.to_string(),
    };
    staged_roots.store_object_admitted(guard, cas, &snapshot.to_value())
}

/// Materialize one immutable snapshot lower tree without copying payload bytes
/// per snapshot or per launch. Content inodes are keyed by blob digest *and*
/// normalized mode; snapshot and launch trees contain only hard links to those
/// daemon-private immutable inodes.
///
/// The returned tree must never be exposed writable. Normal durable execution
/// presents it as the read-only lower of a verified workspace overlay.
pub fn checkout_project_lower(
    authority: &ryeos_state::PinnedStateAuthority,
    cas_mutation_guard: &ryeos_state::CasMutationGuard,
    snapshot_hash: &str,
    target_dir: &Path,
    cache: &MaterializationCache,
) -> Result<(PathBuf, std::fs::File)> {
    authority.ensure_guard(cas_mutation_guard)?;
    let cas = authority.cas_store()?;
    let snapshot_value = cas
        .get_object(snapshot_hash)?
        .ok_or_else(|| anyhow::anyhow!("project snapshot {snapshot_hash} not found"))?;
    let snapshot = ryeos_state::objects::ProjectSnapshot::from_value(&snapshot_value)?;
    let tree_value = cas
        .get_object(&snapshot.project_tree_hash)?
        .ok_or_else(|| anyhow::anyhow!("project tree {} not found", snapshot.project_tree_hash))?;
    let tree = ProjectTree::from_value(&tree_value)?;
    let policy_value = cas
        .get_object(&snapshot.effective_policy_hash)?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "project snapshot policy {} not found",
                snapshot.effective_policy_hash
            )
        })?;
    let policy = ProjectSnapshotPolicy::from_value(&policy_value)?;
    ryeos_state::project_sync::validate_project_tree_paths(&tree, &policy)?;
    ryeos_state::project_sync::validate_captured_policy_source(&cas, &tree, &policy)?;
    let mut project_files = BTreeMap::new();
    for (relative, object_hash) in &tree.files {
        let object = cas
            .get_object(object_hash)?
            .ok_or_else(|| anyhow::anyhow!("project_file object {object_hash} not found"))?;
        project_files.insert(
            relative.clone(),
            ryeos_state::objects::ProjectFile::from_value(&object)?,
        );
    }

    let _build_lock = cache.generation_build_lock(snapshot_hash)?;
    if cache
        .verify_complete_for_tree(&cas, &tree, snapshot_hash)
        .is_err()
    {
        cache.discard_generation(snapshot_hash)?;
        let cache_root = cache.pinned_root()?;
        let staging_name = std::ffi::OsString::from(format!(
            "{snapshot_hash}.staging.{}.{}",
            std::process::id(),
            rand::random::<u32>()
        ));
        let staging_root = cache_root.create_child(&staging_name, 0o700)?;
        let construction = (|| {
            for (relative, project_file) in &project_files {
                let content = cache.ensure_content_file(&cas, project_file)?;
                let (parent, name) = pinned_output_parent(&staging_root, relative)?;
                content.link_to(&parent, &name)?;
            }
            cache.publish_tree(&cache_root, &staging_name, &staging_root, snapshot_hash)
        })();
        if construction.is_err() {
            // A durability-uncertain publication has already moved this
            // descriptor to the final generation name. Only clean it when
            // the original staging pathname still binds to the same inode.
            if staging_root.ensure_path_binding().is_ok() {
                let _ = staging_root.remove_contents_recursive().and_then(|()| {
                    cache_root
                        .remove_empty_child_if_same(&staging_name, &staging_root)
                        .and_then(|removed| {
                            if removed {
                                Ok(())
                            } else {
                                anyhow::bail!("materialization staging remained non-empty")
                            }
                        })
                });
            }
        }
        construction?;
    }
    cache.verify_complete_for_tree(&cas, &tree, snapshot_hash)?;

    let target_root = lillux::secure_fs::PinnedDirectory::open_or_create(target_dir)?;
    for (relative, project_file) in &project_files {
        let content = cache.ensure_content_file(&cas, project_file)?;
        let (parent, name) = pinned_output_parent(&target_root, relative)?;
        content.link_to(&parent, &name)?;
    }
    let lease = cache.generation_lease(snapshot_hash)?;
    drop(_build_lock);
    cache.prune(128)?;
    Ok((target_dir.to_path_buf(), lease))
}

fn pinned_output_parent(
    root: &lillux::secure_fs::PinnedDirectory,
    relative: &str,
) -> Result<(lillux::secure_fs::PinnedDirectory, OsString)> {
    let path = Path::new(relative);
    if path.is_absolute() {
        anyhow::bail!("materialization path must be relative: {relative}");
    }
    let mut components = path.components().peekable();
    let mut parent = root.try_clone()?;
    let mut filename = None;
    while let Some(component) = components.next() {
        let Component::Normal(name) = component else {
            anyhow::bail!("materialization path is not normalized: {relative}");
        };
        if components.peek().is_none() {
            filename = Some(name.to_os_string());
        } else {
            parent = parent.open_or_create_child(name, 0o700)?;
        }
    }
    let filename = filename.ok_or_else(|| anyhow::anyhow!("materialization path is empty"))?;
    if filename == OsStr::new(".") || filename == OsStr::new("..") {
        anyhow::bail!("materialization filename is not a normal component");
    }
    Ok((parent, filename))
}

// ── Fold-back ───────────────────────────────────────────────────────

/// Capture the authoritative post-execution tree under the exact immutable
/// policy that produced the base generation.
pub(crate) struct FoldBackOutputsParams<'a> {
    pub authority: &'a ryeos_state::PinnedStateAuthority,
    pub cas_mutation_guard: &'a ryeos_state::CasMutationGuard,
    pub isolation: &'a ryeos_engine::isolation::IsolationRuntime,
    pub workspace_id: &'a str,
    pub launch_owner: &'a str,
    pub working_dir: &'a Path,
    pub pre_tree_hash: &'a str,
    pub policy_hash: &'a str,
    pub base_snapshot_hash: &'a str,
    pub workspace_record: &'a ryeos_app::runtime_db::WorkspaceRecord,
}

pub(crate) fn fold_back_outputs(
    params: FoldBackOutputsParams<'_>,
) -> Result<(Option<String>, PendingCasPublication)> {
    let FoldBackOutputsParams {
        authority,
        cas_mutation_guard,
        isolation,
        workspace_id,
        launch_owner,
        working_dir,
        pre_tree_hash,
        policy_hash,
        base_snapshot_hash,
        workspace_record,
    } = params;
    authority.ensure_guard(cas_mutation_guard)?;
    let cas = authority.cas_store()?;
    let mut staged_roots = authority
        .require_recovery()?
        .begin_staged_cas_roots_admitted(cas_mutation_guard, "workspace-foldback")?;

    let pre_tree_obj = cas
        .get_object(pre_tree_hash)?
        .ok_or_else(|| anyhow::anyhow!("pre-execution project tree {pre_tree_hash} not found"))?;
    let pre_tree = ProjectTree::from_value(&pre_tree_obj)?;
    let policy_obj = cas
        .get_object(policy_hash)?
        .ok_or_else(|| anyhow::anyhow!("project snapshot policy {policy_hash} not found"))?;
    let policy = ProjectSnapshotPolicy::from_value(&policy_obj)?;
    ryeos_state::project_sync::validate_project_tree_paths(&pre_tree, &policy)?;
    ryeos_state::project_sync::validate_captured_policy_source(&cas, &pre_tree, &policy)?;

    let layout = workspace::WorkspaceLayout::from_root(working_dir.to_path_buf());
    if !layout.lower.is_dir() || !layout.upper.is_dir() || !layout.work.is_dir() {
        anyhow::bail!(
            "authoritative fold-back requires a verified COW workspace, got {}",
            working_dir.display()
        );
    }
    let lifecycle = isolation
        .workspace_lifecycle_pinned(ryeos_engine::isolation::WorkspaceLifecycleInvocation {
            operation: ryeos_isolation_protocol::WorkspaceLifecycleOperation::FreezeAndDiff,
            workspace_id,
            launch_owner,
            lower_snapshot: base_snapshot_hash,
            lower_path: &layout.lower,
            upper_path: &layout.upper,
            work_path: &layout.work,
        })
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    let pinned = lillux::canonical_json(&serde_json::to_value(
        &lifecycle.response.pinned_root_identities,
    )?)?;
    if workspace_record.workspace_id != workspace_id
        || workspace_record.lower_snapshot != base_snapshot_hash
        || workspace_record.launch_owner.as_deref() != Some(launch_owner)
        || workspace_record.backend_id.as_deref() != Some(lifecycle.response.backend_id.as_str())
        || workspace_record.backend_version.as_deref()
            != Some(lifecycle.response.backend_version.as_str())
        || workspace_record.pinned_root_identities.as_deref() != Some(pinned.as_str())
        || workspace_record.mount_identity.as_deref()
            != Some(lifecycle.response.mount_identity.as_str())
    {
        anyhow::bail!("workspace freeze evidence does not match the durable creation journal");
    }
    let Some(new_tree) = workspace::apply_workspace_delta(
        authority,
        cas_mutation_guard,
        &mut staged_roots,
        &lifecycle.upper,
        &pre_tree,
        &policy,
        &lifecycle.response.mutations,
    )?
    else {
        return Ok((
            None,
            PendingCasPublication {
                authority: authority.try_clone()?,
                staged_roots: Some(staged_roots),
            },
        ));
    };
    let new_hash =
        staged_roots.store_object_admitted(cas_mutation_guard, &cas, &new_tree.to_value())?;

    tracing::debug!(
        old_hash = pre_tree_hash,
        new_hash = %new_hash,
        "fold-back produced new project tree"
    );

    Ok((
        Some(new_hash),
        PendingCasPublication {
            authority: authority.try_clone()?,
            staged_roots: Some(staged_roots),
        },
    ))
}

/// Advance the principal-scoped project head ref after fold-back.
///
/// Uses compare-and-swap: `current_snapshot_hash` must match the
/// existing HEAD target, or the operation fails with a conflict error.
/// Returns the new snapshot hash on success.
///
/// The `principal_key` is the raw fingerprint hex (from
/// [`ryeos_state::refs::principal_storage_key`]).
// Pinned authority, held CAS guard, signed head identity, and both snapshot
// hashes remain explicit at the compare-and-swap fold-back boundary.
#[allow(clippy::too_many_arguments)]
pub(crate) fn advance_after_foldback(
    authority: &ryeos_state::PinnedStateAuthority,
    cas_mutation_guard: &ryeos_state::CasMutationGuard,
    state_db: &ryeos_state::StateDb,
    signer: &dyn Signer,
    principal_key: &str,
    project_path_hash: &str,
    new_tree_hash: &str,
    current_snapshot_hash: &str,
    publication: &mut PendingCasPublication,
) -> Result<String> {
    authority.ensure_guard(cas_mutation_guard)?;
    state_db
        .pinned_authority()?
        .ensure_guard(cas_mutation_guard)?;
    let new_snapshot_hash = store_foldback_snapshot(
        authority,
        cas_mutation_guard,
        new_tree_hash,
        current_snapshot_hash,
        publication,
    )?;

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

/// Publish one immutable result generation over a verified workspace delta.
pub(crate) fn store_foldback_snapshot(
    authority: &ryeos_state::PinnedStateAuthority,
    cas_mutation_guard: &ryeos_state::CasMutationGuard,
    new_tree_hash: &str,
    current_snapshot_hash: &str,
    publication: &mut PendingCasPublication,
) -> Result<String> {
    authority.ensure_guard(cas_mutation_guard)?;
    let cas = authority.cas_store()?;
    let current_snapshot_obj = cas.get_object(current_snapshot_hash)?.ok_or_else(|| {
        anyhow::anyhow!(
            "current snapshot {} not found in CAS",
            current_snapshot_hash
        )
    })?;
    let current_snapshot =
        ryeos_state::objects::ProjectSnapshot::from_value(&current_snapshot_obj)?;
    let snapshot = ryeos_state::objects::ProjectSnapshot {
        project_tree_hash: new_tree_hash.to_string(),
        effective_policy_hash: current_snapshot.effective_policy_hash,
        message: None,
        parent_hashes: vec![current_snapshot_hash.to_string()],
        created_at: lillux::time::iso8601_now(),
        source: "workspace_foldback".to_string(),
    };
    publication
        .staged_roots
        .as_mut()
        .expect("pending foldback publication owns staged roots")
        .store_object_admitted(cas_mutation_guard, &cas, &snapshot.to_value())
}

/// Seal the exact generation visible at a synchronous runtime callback
/// barrier. The runtime is blocked in the callback protocol while this runs;
/// it cannot resume until the daemon either rejects the intent or has durably
/// published the generation used for child/successor birth.
pub(crate) fn seal_callback_workspace_generation(
    state: &ryeos_app::state::AppState,
    thread_id: &str,
    effective_lower: &Path,
    base_snapshot_hash: &str,
) -> Result<PendingProjectResult> {
    let authority = pinned_state_authority(state)?;
    let guard = authority.acquire_shared_guard()?;
    let cas = authority.cas_store()?;
    let snapshot_value = cas
        .get_object(base_snapshot_hash)?
        .ok_or_else(|| anyhow::anyhow!("base project snapshot {base_snapshot_hash} is absent"))?;
    let snapshot = ryeos_state::objects::ProjectSnapshot::from_value(&snapshot_value)?;
    let workspace = workspace::WorkspaceLayout::from_lower(effective_lower)?;
    let workspace_id = workspace
        .root
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| anyhow::anyhow!("workspace id is not valid UTF-8"))?;
    let record = state
        .state_store
        .execution_workspace(workspace_id)?
        .ok_or_else(|| anyhow::anyhow!("workspace journal row is missing"))?;
    let launch_owner = record
        .launch_owner
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("workspace has no launch owner"))?;
    state
        .state_store
        .assert_launch_owner(thread_id, launch_owner)?;
    if record.lower_snapshot != base_snapshot_hash {
        anyhow::bail!("callback workspace lower snapshot contradicts its resume base");
    }
    match record.state {
        WorkspaceState::Active => state.state_store.transition_execution_workspace_owned(
            workspace_id,
            thread_id,
            launch_owner,
            &[WorkspaceState::Active],
            WorkspaceState::Freezing,
            None,
        )?,
        WorkspaceState::Freezing => {}
        state => {
            anyhow::bail!("callback workspace {workspace_id} cannot freeze from state {state}")
        }
    }
    let process_identity = state
        .state_store
        .execution_process_identity_owned(thread_id, launch_owner)?;
    let quiesced = QuiescedExecutionGroup::stop(process_identity)?;
    if let Some(snapshot_hash) = record.frozen_snapshot_hash.as_ref() {
        return Ok(PendingProjectResult {
            snapshot_hash: snapshot_hash.clone(),
            publication: None,
            quiesced: Some(quiesced),
        });
    }
    let permit = state
        .write_barrier
        .try_acquire()
        .map_err(|error| anyhow::anyhow!("acquire callback generation write permit: {error}"))?;
    let (next_tree, mut publication) = fold_back_outputs(FoldBackOutputsParams {
        authority: &authority,
        cas_mutation_guard: &guard,
        isolation: &state.isolation,
        workspace_id,
        launch_owner,
        working_dir: &workspace.root,
        pre_tree_hash: &snapshot.project_tree_hash,
        policy_hash: &snapshot.effective_policy_hash,
        base_snapshot_hash,
        workspace_record: &record,
    })?;
    let snapshot_hash = match next_tree {
        Some(tree_hash) => store_foldback_snapshot(
            &authority,
            &guard,
            &tree_hash,
            base_snapshot_hash,
            &mut publication,
        )?,
        None => base_snapshot_hash.to_string(),
    };
    // StateStore owns the same write barrier for its runtime transaction; CAS
    // writes are complete and protected by the staged-root lease at this point.
    drop(permit);
    state
        .state_store
        .assert_launch_owner(thread_id, launch_owner)?;
    state.state_store.bind_frozen_execution_workspace(
        workspace_id,
        thread_id,
        launch_owner,
        &snapshot_hash,
    )?;
    Ok(PendingProjectResult {
        snapshot_hash,
        publication: Some(publication),
        quiesced: Some(quiesced),
    })
}

/// Complete a write-ahead callback freeze whose runtime owner died after the
/// workspace entered `freezing` but before its snapshot binding committed.
/// The dead process makes the upper layer stable; the exact captured adapter
/// replays FreezeAndDiff against the preserved journal identity.
pub fn recover_interrupted_workspace_freeze(
    state: &ryeos_app::state::AppState,
    record: &ryeos_app::runtime_db::WorkspaceRecord,
) -> Result<String> {
    if record.state != WorkspaceState::Freezing {
        anyhow::bail!("only a freezing workspace can recover a callback generation");
    }
    if let Some(snapshot_hash) = record.frozen_snapshot_hash.as_ref() {
        return Ok(snapshot_hash.clone());
    }
    let thread_id = record
        .thread_id
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("freezing workspace has no thread owner"))?;
    let launch_owner = record
        .launch_owner
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("freezing workspace has no launch owner"))?;
    let authority = pinned_state_authority(state)?;
    let guard = authority.acquire_shared_guard()?;
    let cas = authority.cas_store()?;
    let base_value = cas
        .get_object(&record.lower_snapshot)?
        .ok_or_else(|| anyhow::anyhow!("freezing workspace base snapshot is absent"))?;
    let base = ryeos_state::objects::ProjectSnapshot::from_value(&base_value)?;
    let permit = state
        .write_barrier
        .try_acquire()
        .map_err(|error| anyhow::anyhow!("acquire recovery freeze write permit: {error}"))?;
    let (next_tree, mut publication) = fold_back_outputs(FoldBackOutputsParams {
        authority: &authority,
        cas_mutation_guard: &guard,
        isolation: &state.isolation,
        workspace_id: &record.workspace_id,
        launch_owner,
        working_dir: Path::new(&record.root_path),
        pre_tree_hash: &base.project_tree_hash,
        policy_hash: &base.effective_policy_hash,
        base_snapshot_hash: &record.lower_snapshot,
        workspace_record: record,
    })?;
    let snapshot_hash = match next_tree {
        Some(tree_hash) => store_foldback_snapshot(
            &authority,
            &guard,
            &tree_hash,
            &record.lower_snapshot,
            &mut publication,
        )?,
        None => record.lower_snapshot.clone(),
    };
    drop(permit);
    state.state_store.bind_frozen_execution_workspace(
        &record.workspace_id,
        thread_id,
        launch_owner,
        &snapshot_hash,
    )?;
    publication.publish()?;
    Ok(snapshot_hash)
}

pub(crate) struct QuiescedExecutionGroup {
    members: Vec<ryeos_app::process::ExecutionProcessIdentity>,
}

impl QuiescedExecutionGroup {
    fn stop(identity: ryeos_app::process::ExecutionProcessIdentity) -> Result<Self> {
        let outcome = ryeos_app::process::signal_exact_group(&identity, libc::SIGSTOP);
        if outcome != ryeos_app::process::SignalResult::Delivered {
            anyhow::bail!(
                "could not quiesce exact execution group: {}",
                outcome.as_str()
            );
        }
        let members = ryeos_app::process::wait_for_exact_group_quiesced(
            &identity,
            std::time::Duration::from_secs(2),
        )?;
        Ok(Self { members })
    }
}

impl Drop for QuiescedExecutionGroup {
    fn drop(&mut self) {
        for member in &self.members {
            let outcome = ryeos_app::process::signal_exact_target(member, libc::SIGCONT);
            if !matches!(
                outcome,
                ryeos_app::process::SignalResult::Delivered
                    | ryeos_app::process::SignalResult::AlreadyDead
                    | ryeos_app::process::SignalResult::StaleIdentity
            ) {
                tracing::error!(
                    pid = member.target_pid,
                    outcome = outcome.as_str(),
                    "failed to resume an exact quiesced execution-group member"
                );
            }
        }
    }
}

/// Callback handoff resolves already-verified child requests through the
/// parent's immutable launch engine. Runtime data mutations are allowed, but
/// changing `.ai` definitions inside the same segment would make that engine
/// disagree with the frozen generation. Refuse that ambiguous handoff; the
/// author can start a new root from the newly committed generation instead.
pub(crate) fn ensure_control_tree_unchanged(
    state: &ryeos_app::state::AppState,
    before_snapshot_hash: &str,
    after_snapshot_hash: &str,
) -> Result<()> {
    if before_snapshot_hash == after_snapshot_hash {
        return Ok(());
    }
    let read = state.acquire_cas_read()?;
    let load_tree = |snapshot_hash: &str| -> Result<ProjectTree> {
        let value = read
            .cas()
            .get_object(snapshot_hash)?
            .ok_or_else(|| anyhow::anyhow!("project snapshot {snapshot_hash} is absent"))?;
        let snapshot = ryeos_state::objects::ProjectSnapshot::from_value(&value)?;
        let tree = read
            .cas()
            .get_object(&snapshot.project_tree_hash)?
            .ok_or_else(|| {
                anyhow::anyhow!("project tree {} is absent", snapshot.project_tree_hash)
            })?;
        ProjectTree::from_value(&tree)
    };
    let before = load_tree(before_snapshot_hash)?;
    let after = load_tree(after_snapshot_hash)?;
    let before_control = before
        .files
        .iter()
        .filter(|(path, _)| *path == ".ai" || path.starts_with(".ai/"))
        .collect::<std::collections::BTreeMap<_, _>>();
    let after_control = after
        .files
        .iter()
        .filter(|(path, _)| *path == ".ai" || path.starts_with(".ai/"))
        .collect::<std::collections::BTreeMap<_, _>>();
    if before_control != after_control {
        anyhow::bail!(
            "follow handoff changed .ai control files after launch; start the new item from the committed generation"
        );
    }
    Ok(())
}
