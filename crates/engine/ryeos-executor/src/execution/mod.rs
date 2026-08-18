//! Execution lifecycle: checkout, execute, fold-back.
//!
//! Manages the CAS-backed execution flow:
//! 1. Checkout project from CAS to working directory
//! 2. After execution, diff working dir and fold back changes

pub(crate) mod admitted_trust;
pub mod arch_check;
pub mod cache;
pub mod effective_program_projection;
pub(crate) mod execution_realization;
pub(crate) mod external_content;
pub mod ingest;
pub mod launch;
pub(crate) mod launch_claim;
pub mod launch_envelope;
pub mod launch_preparation;
pub mod lillux_bridge;
pub mod limits;
pub mod persistent_session;
pub(crate) mod prepared_launch_cache;
pub(crate) mod process_attachment;
pub mod project_source;
pub mod runner;
pub mod runtime_dispatch;
pub(crate) mod source_closure;
pub mod spawn_detached_child;
pub mod spawn_follow_child;
pub mod thread_meta;
pub mod workspace;

/// Arm node-owned mechanics for fallback copies into private admitted-input
/// roots. The signed policy is loaded by the daemon composition root.
pub fn arm_private_materialization_copy_limit(limit: u64) -> anyhow::Result<()> {
    external_content::arm_private_materialization_copy_limit(limit)
}

use std::ffi::{OsStr, OsString};
use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;

use anyhow::{Context as _, Result};
use ryeos_app::runtime_db::WorkspaceState;

use ryeos_state::objects::ProjectTree;
use ryeos_state::signer::Signer;

use self::cache::MaterializationCache;

/// Project capture/materialization is both filesystem- and CAS-heavy. Keep a
/// small daemon-wide admission window so independent async callbacks cannot
/// amplify one large project into unbounded concurrent copies.
const MAX_CONCURRENT_PROJECT_CAPTURE_WORK: usize = 2;
static PROJECT_CAPTURE_PERMITS: LazyLock<tokio::sync::Semaphore> =
    LazyLock::new(|| tokio::sync::Semaphore::new(MAX_CONCURRENT_PROJECT_CAPTURE_WORK));

pub async fn run_bounded_project_capture<T, E>(
    operation: impl FnOnce() -> std::result::Result<T, E> + Send + 'static,
) -> std::result::Result<T, E>
where
    T: Send + 'static,
    E: Send + 'static,
{
    run_bounded_project_capture_observed(operation, None).await
}

pub async fn run_bounded_project_capture_observed<T, E>(
    operation: impl FnOnce() -> std::result::Result<T, E> + Send + 'static,
    launch_timings: Option<ryeos_app::launch_stage_timings::LaunchStageTimings>,
) -> std::result::Result<T, E>
where
    T: Send + 'static,
    E: Send + 'static,
{
    let semaphore_timer = launch_timings.as_ref().map(|timings| {
        timings.nested(
            "project_context_resolution",
            "project_capture_semaphore_wait",
        )
    });
    let permit = PROJECT_CAPTURE_PERMITS
        .acquire()
        .await
        .expect("static project-capture semaphore is never closed");
    drop(semaphore_timer);
    let result = run_project_capture_off_thread(operation, launch_timings).await;
    drop(permit);
    result
}

/// Run lightweight live-project filesystem resolution off the async worker
/// without consuming one of the scarce CAS capture/materialization permits.
pub async fn run_unbounded_project_capture<T, E>(
    operation: impl FnOnce() -> std::result::Result<T, E> + Send + 'static,
) -> std::result::Result<T, E>
where
    T: Send + 'static,
    E: Send + 'static,
{
    run_unbounded_project_capture_observed(operation, None).await
}

pub async fn run_unbounded_project_capture_observed<T, E>(
    operation: impl FnOnce() -> std::result::Result<T, E> + Send + 'static,
    launch_timings: Option<ryeos_app::launch_stage_timings::LaunchStageTimings>,
) -> std::result::Result<T, E>
where
    T: Send + 'static,
    E: Send + 'static,
{
    run_project_capture_off_thread(operation, launch_timings).await
}

async fn run_project_capture_off_thread<T, E>(
    operation: impl FnOnce() -> std::result::Result<T, E> + Send + 'static,
    launch_timings: Option<ryeos_app::launch_stage_timings::LaunchStageTimings>,
) -> std::result::Result<T, E>
where
    T: Send + 'static,
    E: Send + 'static,
{
    let queue_timer = launch_timings.as_ref().map(|timings| {
        timings.nested(
            "project_context_resolution",
            "project_capture_blocking_queue_wait",
        )
    });
    let result = tokio::task::spawn_blocking(move || {
        drop(queue_timer);
        let _work_timer = launch_timings.as_ref().map(|timings| {
            timings.nested(
                "project_context_resolution",
                "project_capture_blocking_work",
            )
        });
        operation()
    });
    match result.await {
        Ok(result) => result,
        Err(join_error) if join_error.is_panic() => {
            std::panic::resume_unwind(join_error.into_panic())
        }
        Err(join_error) => {
            panic!("project capture blocking task was cancelled: {join_error}")
        }
    }
}

/// A descriptor-pinned CAS publication whose immutable objects are protected
/// by durable recovery roots until a daemon-authoritative consumer is visible.
/// The recovery lease remains live across asynchronous launch; each synchronous
/// mutation phase acquires the shared guard before its write permit and holds
/// both through durable staged-root publication.
pub(crate) type PendingCasPublication = ryeos_state::PendingCasPublication;

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

/// A managed runtime's result generation captured while its terminal callback
/// is still a synchronous execution barrier. The inner staged publication is
/// intentionally opaque outside the executor: the daemon may read the exact
/// snapshot identity and may release its recovery roots only after that same
/// identity is present in authoritative terminal state.
pub struct PreparedManagedRuntimeProjectResult {
    pending: PendingProjectResult,
}

impl PreparedManagedRuntimeProjectResult {
    pub fn snapshot_hash(&self) -> &str {
        self.pending.snapshot_hash()
    }

    pub fn publish(self) -> Result<()> {
        self.pending.publish()
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
    pub(crate) stable_project_identity: ryeos_app::launch_metadata::StableProjectIdentity,
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
        source,
        pending.publication,
    )
}

pub(crate) fn derive_pinned_child_authority(
    parent: &ryeos_state::objects::ExecutionProjectAuthority,
    snapshot_hash: String,
    realization: ryeos_state::objects::PinnedChildProjectRealization,
) -> Result<ryeos_state::objects::ExecutionProjectAuthority> {
    let (stable_identity, display_path, environment, capability_ceiling) = match parent {
        ryeos_state::objects::ExecutionProjectAuthority::LiveProject {
            authored_project_identity,
            canonical_root,
            environment,
            capability_ceiling,
            ..
        } => (
            authored_project_identity.clone(),
            Some(canonical_root.clone()),
            environment.clone(),
            capability_ceiling.clone(),
        ),
        ryeos_state::objects::ExecutionProjectAuthority::PinnedGeneration {
            stable_project_identity,
            display_path,
            environment,
            capability_ceiling,
            ..
        } => (
            stable_project_identity.clone(),
            display_path.clone(),
            environment.clone(),
            capability_ceiling.clone(),
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
        capability_ceiling,
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
        .acquire_with_timeout(ryeos_app::write_barrier::ONLINE_WRITE_PERMIT_TIMEOUT)
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
        publication: PendingCasPublication::new(authority, staged_roots),
    })
}

/// Promote an already-staged project tree to a project snapshot under the
/// same pinned runtime/CAS/recovery authority and durable recovery lease.
pub(crate) fn capture_tree_project_snapshot(
    state: &ryeos_app::state::AppState,
    tree_hash: String,
    policy_hash: String,
    stable_project_identity: ryeos_app::launch_metadata::StableProjectIdentity,
    source: &str,
    mut publication: PendingCasPublication,
) -> Result<CapturedProjectGeneration> {
    let guard = publication.authority().acquire_shared_guard()?;
    publication.authority().ensure_guard(&guard)?;
    let _permit = state
        .write_barrier
        .acquire_with_timeout(ryeos_app::write_barrier::ONLINE_WRITE_PERMIT_TIMEOUT)
        .map_err(|error| anyhow::anyhow!("cannot acquire CAS write permit: {error}"))?;
    let cas = publication.authority().cas_store()?;
    ryeos_state::project_materialization::VerifiedProjectTreeClosure::load(
        &cas,
        &tree_hash,
        &policy_hash,
    )?;
    publication
        .staged_roots_mut()
        .protect_object_hash_admitted(&guard, &tree_hash)?;
    publication
        .staged_roots_mut()
        .protect_object_hash_admitted(&guard, &policy_hash)?;
    let hash = store_project_snapshot(
        publication.staged_roots_mut(),
        &guard,
        &cas,
        tree_hash.clone(),
        policy_hash.clone(),
        source,
    )?;
    Ok(CapturedProjectGeneration {
        snapshot_hash: hash,
        stable_project_identity,
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

/// Select how an immutable snapshot becomes visible to one execution.
///
/// A shared cache and an enforced overlay lower remain read-only, so both may
/// safely share verified content inodes. Disabled isolation executes directly
/// in its daemon-owned workspace, making that tree writable; it must receive
/// independent inodes materialized from CAS instead of links into the immutable
/// cache.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ProjectLowerMaterialization<'a> {
    SharedReadOnly,
    EnforcedOverlayLower(&'a Path),
    PrivateWritableWorkspace {
        target_dir: &'a Path,
        budget: Option<&'a external_content::PrivateMaterializationBudget>,
    },
}

/// Materialize one immutable snapshot for the selected execution boundary.
/// Content inodes in shared/read-only trees are keyed by blob digest and
/// normalized mode. Writable private workspaces receive byte-identical but
/// inode-independent files so one execution can never mutate another
/// execution's snapshot authority.
pub(crate) fn checkout_project_lower(
    authority: &ryeos_state::PinnedStateAuthority,
    cas_mutation_guard: &ryeos_state::CasMutationGuard,
    snapshot_hash: &str,
    materialization: ProjectLowerMaterialization<'_>,
    cache: &MaterializationCache,
) -> Result<(
    PathBuf,
    std::fs::File,
    ryeos_state::PinnedProjectMaterialization,
)> {
    authority.ensure_guard(cas_mutation_guard)?;
    let cas = authority.cas_store()?;
    let closure = ryeos_state::project_materialization::VerifiedProjectSnapshotClosure::load(
        &cas,
        snapshot_hash,
    )?;
    let project_files = closure.tree().files();

    let _build_lock = cache.generation_build_lock(snapshot_hash)?;
    if cache
        .verify_completion_marker_for_files(project_files, snapshot_hash)
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
            for (relative, project_file) in project_files {
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
    let realized_path = match materialization {
        ProjectLowerMaterialization::SharedReadOnly => cache.cache_dir(snapshot_hash),
        ProjectLowerMaterialization::EnforcedOverlayLower(target_dir) => {
            let target_root = lillux::secure_fs::PinnedDirectory::open_or_create(target_dir)?;
            for (relative, project_file) in project_files {
                let content = cache.ensure_content_file(&cas, project_file)?;
                let (parent, name) = pinned_output_parent(&target_root, relative)?;
                content.link_to(&parent, &name)?;
            }
            target_dir.to_path_buf()
        }
        ProjectLowerMaterialization::PrivateWritableWorkspace { target_dir, budget } => {
            let target_root = lillux::secure_fs::PinnedDirectory::open_or_create(target_dir)?;
            let owned_budget;
            let budget = match budget {
                Some(budget) => budget,
                None => {
                    owned_budget = external_content::private_materialization_budget()?;
                    &owned_budget
                }
            };
            for (relative, project_file) in project_files {
                let content = cache.ensure_content_file(&cas, project_file)?;
                let (parent, name) = pinned_output_parent(&target_root, relative)?;
                budget.materialize_regular(
                    &parent,
                    &name,
                    content.descriptor(),
                    project_file.size,
                    project_file.normalized_mode,
                )?;
            }
            target_dir.to_path_buf()
        }
    };
    let materialization = match ryeos_state::PinnedProjectMaterialization::verify_from_closure(
        authority,
        cas_mutation_guard,
        &closure,
        &realized_path,
    ) {
        Ok(materialization) => materialization,
        Err(error) if matches!(materialization, ProjectLowerMaterialization::SharedReadOnly) => {
            // A valid marker beside a mutated generation is not authority.
            // Rebuild once beneath the still-held construction lock, then
            // mint the proof from the rebuilt descriptor tree.
            cache.discard_generation(snapshot_hash)?;
            let cache_root = cache.pinned_root()?;
            let staging_name = std::ffi::OsString::from(format!(
                "{snapshot_hash}.staging.{}.{}",
                std::process::id(),
                rand::random::<u32>()
            ));
            let staging_root = cache_root.create_child(&staging_name, 0o700)?;
            for (relative, project_file) in project_files {
                let content = cache.ensure_content_file(&cas, project_file)?;
                let (parent, name) = pinned_output_parent(&staging_root, relative)?;
                content.link_to(&parent, &name)?;
            }
            cache.publish_tree(&cache_root, &staging_name, &staging_root, snapshot_hash)?;
            ryeos_state::PinnedProjectMaterialization::verify_from_closure(
                authority,
                cas_mutation_guard,
                &closure,
                &realized_path,
            )
            .with_context(|| {
                format!(
                    "rebuilt materialization remained invalid after prior verification failure: {error:#}"
                )
            })?
        }
        Err(error) => return Err(error),
    };
    let lease = cache.generation_lease(snapshot_hash)?;
    drop(_build_lock);
    cache.prune(128)?;
    Ok((realized_path, lease, materialization))
}

pub(super) fn pinned_output_parent(
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

fn admitted_operational_shadow_paths(
    state: &ryeos_app::state::AppState,
    thread_id: &str,
) -> Result<Vec<String>> {
    let Some(evidence) = state.state_store.admitted_program_evidence(thread_id)? else {
        return Ok(Vec::new());
    };
    let mut paths = external_content::admitted_realization_mounts(&evidence.resolution)?;
    if let Some(source_mount) = source_closure::admitted_source_mount(state, &evidence.resolution)?
    {
        paths.push(source_mount);
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}

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
    pub operational_shadow_paths: &'a [String],
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
        operational_shadow_paths,
    } = params;
    authority.ensure_guard(cas_mutation_guard)?;
    let cas = authority.cas_store()?;
    let mut staged_roots = authority
        .require_recovery()?
        .begin_staged_cas_roots_admitted(cas_mutation_guard, "workspace-foldback")?;

    let closure = ryeos_state::project_materialization::VerifiedProjectTreeClosure::load(
        &cas,
        pre_tree_hash,
        policy_hash,
    )?;
    let pre_tree = closure.tree();
    let policy = closure.policy();

    let layout = workspace::WorkspaceLayout::from_root(working_dir.to_path_buf());
    if !layout.lower.is_dir() || !layout.upper.is_dir() || !layout.work.is_dir() {
        anyhow::bail!(
            "authoritative fold-back requires a verified COW workspace, got {}",
            working_dir.display()
        );
    }
    let lifecycle_operation = if isolation.is_enforced() {
        ryeos_isolation_protocol::WorkspaceLifecycleOperation::FreezeAndDiff
    } else {
        // A disabled node has no mount namespace or overlay adapter. Re-run
        // the exact native Create check to pin the same private directories,
        // then capture the complete mutable lower tree below.
        ryeos_isolation_protocol::WorkspaceLifecycleOperation::Create
    };
    let lifecycle = isolation
        .workspace_lifecycle_pinned(ryeos_engine::isolation::WorkspaceLifecycleInvocation {
            operation: lifecycle_operation,
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
    let new_tree = if isolation.is_enforced() {
        workspace::apply_workspace_delta(
            authority,
            cas_mutation_guard,
            &mut staged_roots,
            &lifecycle.upper,
            pre_tree,
            policy,
            &lifecycle.response.mutations,
        )?
    } else {
        let lower = lillux::PinnedDirectory::open(&layout.lower)?
            .ok_or_else(|| anyhow::anyhow!("daemon-private workspace lower disappeared"))?;
        let captured = ingest::ingest_project_tree_with_operational_exclusions(
            authority,
            cas_mutation_guard,
            &lower,
            policy,
            operational_shadow_paths,
        )?;
        (captured != *pre_tree).then_some(captured)
    };
    let Some(new_tree) = new_tree else {
        return Ok((
            None,
            PendingCasPublication::new(authority.try_clone()?, staged_roots),
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
        PendingCasPublication::new(authority.try_clone()?, staged_roots),
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
    snapshot_parent_hash: &str,
    expected_head_hash: &str,
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
        snapshot_parent_hash,
        publication,
    )?;

    state_db.advance_project_head_ref(
        principal_key,
        project_path_hash,
        &new_snapshot_hash,
        expected_head_hash,
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
    let current_snapshot = ryeos_state::project_materialization::load_project_snapshot_bounded(
        &cas,
        current_snapshot_hash,
    )?
    .ok_or_else(|| {
        anyhow::anyhow!(
            "current snapshot {} not found in CAS",
            current_snapshot_hash
        )
    })?;
    let snapshot = ryeos_state::objects::ProjectSnapshot {
        project_tree_hash: new_tree_hash.to_string(),
        effective_policy_hash: current_snapshot.effective_policy_hash,
        message: None,
        parent_hashes: vec![current_snapshot_hash.to_string()],
        created_at: lillux::time::iso8601_now(),
        source: "workspace_foldback".to_string(),
    };
    publication.staged_roots_mut().store_object_admitted(
        cas_mutation_guard,
        &cas,
        &snapshot.to_value(),
    )
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
    let snapshot = ryeos_state::project_materialization::load_project_snapshot_bounded(
        &cas,
        base_snapshot_hash,
    )?
    .ok_or_else(|| anyhow::anyhow!("base project snapshot {base_snapshot_hash} is absent"))?;
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
        .acquire_with_timeout(ryeos_app::write_barrier::ONLINE_WRITE_PERMIT_TIMEOUT)
        .map_err(|error| anyhow::anyhow!("acquire callback generation write permit: {error}"))?;
    let operational_shadow_paths = admitted_operational_shadow_paths(state, thread_id)?;
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
        operational_shadow_paths: &operational_shadow_paths,
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

/// Seal the result generation required by a managed runtime's terminal
/// project authority. This runs before terminal state commits and while the
/// runtime is blocked in its authenticated callback, so no process in the
/// execution group can mutate the generation between capture and admission.
///
/// `Discard` owns a COW workspace but deliberately publishes no generation;
/// its workspace is destroyed after the runtime process has exited. Borrowed
/// children never settle the workspace owned by their parent.
pub async fn prepare_managed_runtime_terminal_project_result(
    state: &ryeos_app::state::AppState,
    capability: &ryeos_app::callback_token::CallbackCapability,
    reported_status: &ryeos_engine::contracts::ThreadTerminalStatus,
) -> Result<Option<PreparedManagedRuntimeProjectResult>> {
    let provenance = &capability.provenance;
    if provenance.is_borrowed_child() || !provenance.project_authority().requires_project_foldback()
    {
        return Ok(None);
    }
    let terminal_publication = provenance
        .project_authority()
        .terminal_publication()
        .ok_or_else(|| {
            anyhow::anyhow!("managed COW runtime has no terminal publication authority")
        })?
        .clone();
    if matches!(
        terminal_publication,
        ryeos_state::objects::PinnedTerminalPublication::Discard
    ) {
        return Ok(None);
    }

    let base_snapshot_hash = provenance
        .pinned_snapshot_hash()
        .ok_or_else(|| anyhow::anyhow!("managed COW runtime has no admitted base generation"))?
        .to_string();
    let effective_path = provenance.effective_path().to_path_buf();
    let thread_id = capability.thread_id.clone();
    let launch_owner = capability
        .launch_owner
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("managed runtime callback has no launch owner"))?
        .to_string();
    state
        .state_store
        .assert_launch_owner(&thread_id, &launch_owner)?;

    let capture_state = state.clone();
    let capture_thread_id = thread_id.clone();
    let pending = run_bounded_project_capture(move || {
        seal_callback_workspace_generation(
            &capture_state,
            &capture_thread_id,
            &effective_path,
            &base_snapshot_hash,
        )
    })
    .await?;

    let authoritative_status = state
        .threads
        .get_thread(&thread_id)?
        .ok_or_else(|| anyhow::anyhow!("managed runtime thread disappeared during finalization"))?
        .status;
    let is_continuation_segment = *reported_status
        == ryeos_engine::contracts::ThreadTerminalStatus::Continued
        || authoritative_status == ryeos_state::objects::ThreadStatus::Continued.as_str();
    if !is_continuation_segment
        && let ryeos_state::objects::PinnedTerminalPublication::AdvanceHead {
            head_ref,
            expected_hash,
        } = &terminal_publication
    {
        advance_head_to_frozen_runtime_result(
            state,
            &thread_id,
            &launch_owner,
            head_ref,
            expected_hash,
            pending.snapshot_hash(),
        )?;
    }

    Ok(Some(PreparedManagedRuntimeProjectResult { pending }))
}

/// Seal a retained generation after the supervisor has proved the managed
/// runtime process dead but before fallback terminal state is written. This is
/// the crash/timeout counterpart to the synchronous callback barrier above;
/// it must never be used while an execution process remains attached.
pub(crate) fn prepare_stopped_managed_runtime_terminal_project_result(
    state: &ryeos_app::state::AppState,
    provenance: &ryeos_app::execution_provenance::ExecutionProvenance,
    thread_id: &str,
    launch_owner: &str,
) -> Result<Option<String>> {
    if provenance.is_borrowed_child() || !provenance.project_authority().requires_project_foldback()
    {
        return Ok(None);
    }
    let terminal_publication = provenance
        .project_authority()
        .terminal_publication()
        .ok_or_else(|| {
            anyhow::anyhow!("managed COW runtime has no terminal publication authority")
        })?;
    if matches!(
        terminal_publication,
        ryeos_state::objects::PinnedTerminalPublication::Discard
    ) {
        return Ok(None);
    }
    state
        .state_store
        .assert_execution_process_detached_owned(thread_id, launch_owner)?;

    let layout = workspace::WorkspaceLayout::from_lower(provenance.effective_path())?;
    let workspace_id = layout
        .root
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow::anyhow!("execution workspace id is not valid UTF-8"))?;
    let mut record = state
        .state_store
        .execution_workspace(workspace_id)?
        .ok_or_else(|| anyhow::anyhow!("execution workspace journal row is missing"))?;
    if record.thread_id.as_deref() != Some(thread_id)
        || record.launch_owner.as_deref() != Some(launch_owner)
    {
        anyhow::bail!("stopped managed workspace belongs to another execution owner");
    }
    match record.state {
        WorkspaceState::Ready | WorkspaceState::Active => {
            state.state_store.transition_execution_workspace_owned(
                workspace_id,
                thread_id,
                launch_owner,
                &[record.state],
                WorkspaceState::Freezing,
                None,
            )?;
            record = state
                .state_store
                .execution_workspace(workspace_id)?
                .ok_or_else(|| anyhow::anyhow!("execution workspace disappeared while freezing"))?;
        }
        WorkspaceState::Freezing => {}
        state => anyhow::bail!("stopped managed workspace cannot freeze from state {state}"),
    }
    let snapshot_hash = recover_interrupted_workspace_freeze(state, &record)?;
    if let ryeos_state::objects::PinnedTerminalPublication::AdvanceHead {
        head_ref,
        expected_hash,
    } = terminal_publication
    {
        advance_head_to_frozen_runtime_result(
            state,
            thread_id,
            launch_owner,
            head_ref,
            expected_hash,
            &snapshot_hash,
        )?;
    }
    Ok(Some(snapshot_hash))
}

fn advance_head_to_frozen_runtime_result(
    state: &ryeos_app::state::AppState,
    thread_id: &str,
    launch_owner: &str,
    head_ref: &str,
    expected_hash: &str,
    result_snapshot_hash: &str,
) -> Result<()> {
    let mut components = head_ref.split('/');
    let (Some("projects"), Some(principal_key), Some(project_hash), Some("head"), None) = (
        components.next(),
        components.next(),
        components.next(),
        components.next(),
        components.next(),
    ) else {
        anyhow::bail!("managed runtime advance-head authority has a non-canonical ref");
    };
    let canonical_component = |value: &str| {
        value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    };
    if !canonical_component(principal_key) || !canonical_component(project_hash) {
        anyhow::bail!("managed runtime advance-head authority has a non-canonical identity");
    }

    let authority = pinned_state_authority(state)?;
    let cas_guard = authority.acquire_shared_guard()?;
    authority.ensure_guard(&cas_guard)?;
    let _permit = state
        .write_barrier
        .acquire_with_timeout(ryeos_app::write_barrier::ONLINE_WRITE_PERMIT_TIMEOUT)
        .map_err(|error| anyhow::anyhow!("acquire result-generation write permit: {error}"))?;
    let signer = ryeos_app::state_store::NodeIdentitySigner::from_identity(&state.identity);
    state
        .state_store
        .with_state_db_owned(thread_id, launch_owner, |db| {
            let current = db
                .read_project_head(principal_key, project_hash)?
                .ok_or_else(|| anyhow::anyhow!("managed runtime project HEAD is absent"))?;
            if current == result_snapshot_hash {
                return Ok(());
            }
            if current != expected_hash {
                anyhow::bail!(
                    "managed runtime project HEAD conflict: expected {expected_hash}, got {current}"
                );
            }
            db.advance_project_head_ref(
                principal_key,
                project_hash,
                result_snapshot_hash,
                expected_hash,
                &signer,
                &cas_guard,
            )
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
    let base = ryeos_state::project_materialization::load_project_snapshot_bounded(
        &cas,
        &record.lower_snapshot,
    )?
    .ok_or_else(|| anyhow::anyhow!("freezing workspace base snapshot is absent"))?;
    let permit = state
        .write_barrier
        .acquire_with_timeout(ryeos_app::write_barrier::ONLINE_WRITE_PERMIT_TIMEOUT)
        .map_err(|error| anyhow::anyhow!("acquire recovery freeze write permit: {error}"))?;
    let operational_shadow_paths = admitted_operational_shadow_paths(state, thread_id)?;
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
        operational_shadow_paths: &operational_shadow_paths,
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
        let snapshot = ryeos_state::project_materialization::load_project_snapshot_bounded(
            read.cas(),
            snapshot_hash,
        )?
        .ok_or_else(|| anyhow::anyhow!("project snapshot {snapshot_hash} is absent"))?;
        ryeos_state::project_materialization::load_project_tree_bounded(
            read.cas(),
            &snapshot.project_tree_hash,
        )?
        .ok_or_else(|| anyhow::anyhow!("project tree {} is absent", snapshot.project_tree_hash))
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

#[cfg(test)]
mod pinned_child_authority_tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use ryeos_state::objects::{
        ChildProjectAuthorityPolicy, EnvironmentAuthority, ExecutionProjectAuthority,
        LiveFilesystemConfinement, LiveProjectAccess, PinnedChildProjectRealization, ProjectFile,
        ProjectSnapshot, ProjectSnapshotPolicy, ProjectTree,
    };

    #[cfg(unix)]
    #[test]
    fn writable_private_workspaces_do_not_share_snapshot_cache_inodes() {
        use std::os::unix::fs::MetadataExt as _;

        let state_root = tempfile::tempdir().unwrap();
        let state_db =
            ryeos_state::StateDb::open(state_root.path(), Arc::new(ryeos_state::TrustStore::new()))
                .unwrap();
        let authority = state_db.pinned_authority().unwrap();
        let guard = authority.acquire_shared_guard().unwrap();
        let cas = authority.cas_store().unwrap();

        let bytes = b"immutable snapshot bytes\n";
        let blob_hash = cas.store_blob(bytes).unwrap();
        let project_file = ProjectFile {
            blob_hash,
            size: bytes.len() as u64,
            normalized_mode: ProjectFile::REGULAR_MODE,
        };
        let file_hash = cas.store_object(&project_file.to_value()).unwrap();
        let tree = ProjectTree {
            files: BTreeMap::from([("vendor/runtime/stable.txt".to_owned(), file_hash)]),
        };
        let tree_hash = cas.store_object(&tree.to_value()).unwrap();
        let policy = ProjectSnapshotPolicy::from_matcher(
            ryeos_state::project_sync::ProjectSyncScope::FullProject,
            &ryeos_state::ignore::matcher_from_builtins(),
        )
        .unwrap();
        let policy_hash = cas.store_object(&policy.to_value()).unwrap();
        let snapshot = ProjectSnapshot {
            project_tree_hash: tree_hash,
            effective_policy_hash: policy_hash,
            message: None,
            parent_hashes: Vec::new(),
            created_at: "2026-08-11T00:00:00Z".to_owned(),
            source: "private-workspace-test".to_owned(),
        };
        let snapshot_hash = cas.store_object(&snapshot.to_value()).unwrap();
        let cache_root = tempfile::tempdir().unwrap();
        let cache = MaterializationCache::new(cache_root.path().to_path_buf());

        let (shared_path, _shared_lease, shared) = checkout_project_lower(
            &authority,
            &guard,
            &snapshot_hash,
            ProjectLowerMaterialization::SharedReadOnly,
            &cache,
        )
        .unwrap();
        let first_root = tempfile::tempdir().unwrap();
        let second_root = tempfile::tempdir().unwrap();
        let first_budget = external_content::PrivateMaterializationBudget::new(1024);
        let second_budget = external_content::PrivateMaterializationBudget::new(1024);
        let (first_path, _first_lease, _first) = checkout_project_lower(
            &authority,
            &guard,
            &snapshot_hash,
            ProjectLowerMaterialization::PrivateWritableWorkspace {
                target_dir: first_root.path(),
                budget: Some(&first_budget),
            },
            &cache,
        )
        .unwrap();
        let (second_path, _second_lease, second) = checkout_project_lower(
            &authority,
            &guard,
            &snapshot_hash,
            ProjectLowerMaterialization::PrivateWritableWorkspace {
                target_dir: second_root.path(),
                budget: Some(&second_budget),
            },
            &cache,
        )
        .unwrap();

        let relative = Path::new("vendor/runtime/stable.txt");
        let shared_file = shared_path.join(relative);
        let first_file = first_path.join(relative);
        let second_file = second_path.join(relative);
        assert_ne!(
            std::fs::metadata(&shared_file).unwrap().ino(),
            std::fs::metadata(&first_file).unwrap().ino(),
            "a writable workspace must not share the immutable cache inode"
        );
        assert_ne!(
            std::fs::metadata(&first_file).unwrap().ino(),
            std::fs::metadata(&second_file).unwrap().ino(),
            "two writable workspaces must not share one another's inode"
        );

        std::fs::write(&first_file, b"first workspace mutation\n").unwrap();
        assert_eq!(std::fs::read(&shared_file).unwrap(), bytes);
        assert_eq!(std::fs::read(&second_file).unwrap(), bytes);
        shared.ensure_path_binding().unwrap();
        second.ensure_path_binding().unwrap();
    }

    #[test]
    fn pin_at_spawn_preserves_the_sealed_parent_capability_ceiling() {
        let root = tempfile::tempdir().unwrap();
        let parent = ExecutionProjectAuthority::live(
            root.path().canonicalize().unwrap(),
            "project:test".to_string(),
            LiveProjectAccess::ReadWrite,
            LiveFilesystemConfinement::standard_descriptor_rooted(),
            EnvironmentAuthority::None,
            vec!["sealed.project.cap".to_string()],
        )
        .unwrap()
        .with_child_policy(ChildProjectAuthorityPolicy::PinAtSpawn {
            realization: PinnedChildProjectRealization::ReadOnly,
        })
        .unwrap();

        let child = derive_pinned_child_authority(
            &parent,
            "a".repeat(64),
            PinnedChildProjectRealization::ReadOnly,
        )
        .unwrap();
        let ExecutionProjectAuthority::PinnedGeneration {
            capability_ceiling, ..
        } = child
        else {
            panic!("pin-at-spawn must produce pinned authority");
        };
        assert_eq!(capability_ceiling, vec!["sealed.project.cap".to_string()]);
    }
}
