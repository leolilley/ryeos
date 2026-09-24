//! Execution lifecycle: checkout, execute, fold-back.
//!
//! Manages the CAS-backed execution flow:
//! 1. Checkout project from CAS to working directory
//! 2. After execution, diff working dir and fold back changes

pub(crate) mod admitted_trust;
pub mod arch_check;
pub mod cache;
mod direct_output;
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
pub(crate) mod prepared_content_identity;
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
pub(crate) mod workspace_outputs;

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

/// A source/output generation protected by one temporary publication lease.
/// The caller roots the complete pair before releasing the lease. Dropping an
/// uncommitted result does not constitute successful publication.
pub(crate) struct PendingProjectResult {
    pub(crate) generation: ryeos_state::objects::WorkspaceGenerationPair,
    pub(crate) publication: Option<PendingCasPublication>,
    pub(crate) quiesced: Option<QuiescedExecutionGroup>,
}

impl PendingProjectResult {
    pub(crate) fn snapshot_hash(&self) -> &str {
        &self.generation.snapshot_hash
    }

    pub(crate) fn generation(&self) -> &ryeos_state::objects::WorkspaceGenerationPair {
        &self.generation
    }

    pub(crate) fn publish(mut self) -> Result<()> {
        if let Some(publication) = self.publication.take() {
            publication.publish()?;
        }
        self.quiesced.take();
        Ok(())
    }

    pub(crate) fn into_unpublished_generation_and_quiesced(
        mut self,
    ) -> Result<(
        ryeos_state::objects::WorkspaceGenerationPair,
        Option<PendingCasPublication>,
        QuiescedExecutionGroup,
    )> {
        // The caller must publish while it still owns the quiesced process
        // group. Returning both move-only authorities together prevents a
        // failed publication from implicitly resuming the root while the
        // durable operation remains `quiesced`.
        let publication = self.publication.take();
        let quiesced = self
            .quiesced
            .take()
            .ok_or_else(|| anyhow::anyhow!("captured workspace input lost its quiesced group"))?;
        Ok((self.generation, publication, quiesced))
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

    pub fn generation(&self) -> &ryeos_state::objects::WorkspaceGenerationPair {
        self.pending.generation()
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
    state.state_store.pinned_state_authority()
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
            ryeos_state::objects::PinnedChildProjectRealization::CowRetainResult => {
                ryeos_state::objects::PinnedProjectRealization::Cow {
                    terminal_publication:
                        ryeos_state::objects::PinnedTerminalPublication::RetainResult,
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
/// A shared cache and the canonical project supplied to an enforced CoW
/// backend remain read-only, so both may safely share verified content inodes.
/// Disabled isolation executes directly in its daemon-owned project, making
/// that tree writable; it must receive independent inodes materialized from
/// CAS instead of links into the immutable cache.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ProjectMaterialization<'a> {
    SharedReadOnly,
    EnforcedCowProject(&'a Path),
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
pub(crate) fn checkout_project_snapshot(
    authority: &ryeos_state::PinnedStateAuthority,
    cas_mutation_guard: &ryeos_state::CasMutationGuard,
    snapshot_hash: &str,
    materialization: ProjectMaterialization<'_>,
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
    tracing::debug!(
        snapshot_hash,
        checkout_stage = "closure-loaded",
        "project checkout stage"
    );
    let project_files = closure.tree().files();

    let _build_lock = cache.generation_build_lock(snapshot_hash)?;
    tracing::debug!(
        snapshot_hash,
        checkout_stage = "build-lock-acquired",
        "project checkout stage"
    );
    if cache
        .verify_completion_marker_for_files(project_files, snapshot_hash)
        .is_err()
    {
        cache.discard_generation(snapshot_hash)?;
        let cache_root = cache.pinned_root()?;
        let (staging_name, staging_root) =
            cache_root.create_unique_child(&format!("{snapshot_hash}.staging"), 0o700)?;
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
    tracing::debug!(
        snapshot_hash,
        checkout_stage = "generation-available",
        "project checkout stage"
    );
    let realized_path = match materialization {
        ProjectMaterialization::SharedReadOnly => cache.cache_dir(snapshot_hash),
        ProjectMaterialization::EnforcedCowProject(target_dir) => {
            let target_root = lillux::secure_fs::PinnedDirectory::open_or_create(target_dir)?;
            for (relative, project_file) in project_files {
                let content = cache.ensure_content_file(&cas, project_file)?;
                let (parent, name) = pinned_output_parent(&target_root, relative)?;
                content.link_to(&parent, &name)?;
            }
            target_dir.to_path_buf()
        }
        ProjectMaterialization::PrivateWritableWorkspace { target_dir, budget } => {
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
        Err(error) if matches!(materialization, ProjectMaterialization::SharedReadOnly) => {
            // A valid marker beside a mutated generation is not authority.
            // Rebuild once beneath the still-held construction lock, then
            // mint the proof from the rebuilt descriptor tree.
            cache.discard_generation(snapshot_hash)?;
            let cache_root = cache.pinned_root()?;
            let (staging_name, staging_root) =
                cache_root.create_unique_child(&format!("{snapshot_hash}.staging"), 0o700)?;
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
    tracing::debug!(
        snapshot_hash,
        checkout_stage = "materialization-verified",
        "project checkout stage"
    );
    let lease = cache.generation_lease(snapshot_hash)?;
    tracing::debug!(
        snapshot_hash,
        checkout_stage = "lease-acquired",
        "project checkout stage"
    );
    drop(_build_lock);
    tracing::debug!(
        snapshot_hash,
        checkout_stage = "cache-prune",
        "project checkout stage"
    );
    cache.prune(128)?;
    tracing::debug!(
        snapshot_hash,
        checkout_stage = "cache-pruned",
        "project checkout stage"
    );
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
    capsule: &ryeos_state::objects::AdmittedLaunchCapsule,
) -> Result<Vec<String>> {
    let request: ryeos_app::thread_lifecycle::SealedRootExecutionRequest =
        serde_json::from_value(capsule.sealed_invocation.clone())?;
    let resolution = request.admitted_effective_resolution()?;
    let mut paths = external_content::admitted_realization_mounts(resolution)?;
    if source_closure::capsule_source_placement(
        &capsule.execution_closure,
        state.isolation.is_enforced(),
    )? == source_closure::SourceMountPlacement::Project
    {
        if let Some(source_mount) = source_closure::admitted_source_mount(state, resolution)? {
            paths.push(source_mount);
        }
    }
    if let ryeos_state::objects::AdmittedExecutionClosure::ManagedRuntime {
        prepared_runtime_launch,
        ..
    } = &capsule.execution_closure
    {
        let prepared: launch_preparation::PreparedRuntimeLaunch =
            serde_json::from_value(prepared_runtime_launch.clone())?;
        for binding in prepared.evidence_attachments {
            binding.validate()?;
            paths.push(binding.destination_path);
        }
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
    pub output_partition: Option<&'a ryeos_state::objects::WorkspaceOutputPartition>,
}

pub(crate) struct FoldBackCapture {
    pub tree_hash: Option<String>,
    pub outputs: Option<
        std::collections::BTreeMap<String, ryeos_state::objects::WorkspaceOutputCaptureState>,
    >,
    pub publication: PendingCasPublication,
}

struct WorkspaceOutputCaptureContext {
    partition: ryeos_state::objects::WorkspaceOutputPartition,
    producer_chain_root_id: String,
    producer_thread_id: String,
    admitted_launch_capsule_hash: String,
}

fn workspace_output_capture_context(
    state: &ryeos_app::state::AppState,
    thread_id: &str,
    record: &ryeos_app::runtime_db::WorkspaceRecord,
) -> Result<Option<WorkspaceOutputCaptureContext>> {
    Ok(admitted_workspace_capture_inputs(state, thread_id, record)?.1)
}

/// One verified capsule supplies both exclusion and output authority. No
/// mutable workspace scan or repeated full capsule verification under the
/// state-store mutex is needed to derive these immutable projections.
fn admitted_workspace_capture_inputs(
    state: &ryeos_app::state::AppState,
    thread_id: &str,
    record: &ryeos_app::runtime_db::WorkspaceRecord,
) -> Result<(Vec<String>, Option<WorkspaceOutputCaptureContext>)> {
    let (chain_root_id, capsule_hash, capsule) = state
        .state_store
        .admitted_launch_capsule_with_coordinates(thread_id)?
        .ok_or_else(|| anyhow::anyhow!("workspace capture lost its admitted capsule"))?;
    let paths = admitted_operational_shadow_paths(state, &capsule)?;
    let Some(outputs) = capsule.project_authority.workspace_outputs() else {
        if record.workspace_output_partition_identity.is_some()
            || record.base_output_capture_hash.is_some()
        {
            anyhow::bail!("ordinary workspace journal contains output authority");
        }
        return Ok((paths, None));
    };
    if record.workspace_output_partition_identity.as_deref()
        != Some(outputs.partition.partition_identity.as_str())
        || record.base_output_capture_hash != outputs.capture_hash
        || capsule.project_authority.operational_snapshot_projection()
            != Some(record.base_snapshot.as_str())
    {
        anyhow::bail!("workspace output journal contradicts the admitted source/output generation");
    }
    Ok((
        paths,
        Some(WorkspaceOutputCaptureContext {
            partition: outputs.partition.clone(),
            producer_chain_root_id: chain_root_id,
            producer_thread_id: thread_id.to_owned(),
            admitted_launch_capsule_hash: capsule_hash,
        }),
    ))
}

/// Store the output half only after the source snapshot is known, in the same
/// guarded publication lease. The caller must atomically root the pair in the
/// existing workspace/operation transaction before retiring that lease.
fn store_workspace_output_capture(
    authority: &ryeos_state::PinnedStateAuthority,
    guard: &ryeos_state::CasMutationGuard,
    context: Option<&WorkspaceOutputCaptureContext>,
    outputs: Option<
        std::collections::BTreeMap<String, ryeos_state::objects::WorkspaceOutputCaptureState>,
    >,
    base_snapshot: &str,
    result_snapshot: &str,
    publication: &mut PendingCasPublication,
) -> Result<Option<String>> {
    let (context, outputs) = match (context, outputs) {
        (None, None) => return Ok(None),
        (Some(context), Some(outputs)) => (context, outputs),
        _ => anyhow::bail!("workspace capture produced an incomplete source/output pair"),
    };
    authority.ensure_guard(guard)?;
    let cas = authority.cas_store()?;
    let base =
        ryeos_state::project_materialization::load_project_snapshot_bounded(&cas, base_snapshot)?
            .ok_or_else(|| anyhow::anyhow!("workspace capture base snapshot is absent"))?;
    let result =
        ryeos_state::project_materialization::load_project_snapshot_bounded(&cas, result_snapshot)?
            .ok_or_else(|| anyhow::anyhow!("workspace capture result snapshot is absent"))?;
    let policy = ryeos_state::project_materialization::load_project_policy_bounded(
        &cas,
        &context.partition.project_snapshot_policy_hash,
    )?
    .ok_or_else(|| anyhow::anyhow!("workspace capture policy is absent"))?;
    context
        .partition
        .validate_source_output_pair(&base, &result, &policy)?;
    let capture = ryeos_state::objects::WorkspaceOutputCapture {
        schema: ryeos_state::objects::WORKSPACE_OUTPUT_CAPTURE_SCHEMA.to_owned(),
        kind: ryeos_state::objects::WORKSPACE_OUTPUT_CAPTURE_KIND.to_owned(),
        producer_chain_root_id: context.producer_chain_root_id.clone(),
        producer_thread_id: context.producer_thread_id.clone(),
        admitted_launch_capsule_hash: context.admitted_launch_capsule_hash.clone(),
        base_project_snapshot_hash: base_snapshot.to_owned(),
        result_project_snapshot_hash: result_snapshot.to_owned(),
        partition: context.partition.clone(),
        outputs,
    };
    Ok(Some(publication.staged_roots_mut().store_object_admitted(
        guard,
        &cas,
        &capture.to_value()?,
    )?))
}

fn verified_frozen_generation(
    authority: &ryeos_state::PinnedStateAuthority,
    record: &ryeos_app::runtime_db::WorkspaceRecord,
    context: Option<&WorkspaceOutputCaptureContext>,
) -> Result<Option<ryeos_state::objects::WorkspaceGenerationPair>> {
    let Some(snapshot_hash) = &record.frozen_snapshot_hash else {
        if record.frozen_output_capture_hash.is_some() {
            anyhow::bail!("workspace has output capture without its frozen source snapshot");
        }
        return Ok(None);
    };
    let generation = ryeos_state::objects::WorkspaceGenerationPair {
        snapshot_hash: snapshot_hash.clone(),
        output_capture_hash: record.frozen_output_capture_hash.clone(),
    };
    generation.validate()?;
    match (context, generation.output_capture_hash.as_deref()) {
        (None, None) => {}
        (Some(context), Some(hash)) => {
            let cas = authority.cas_store()?;
            let value = ryeos_state::object_closure::load_exact_cas_object_with_cas(
                &cas,
                hash,
                ryeos_state::objects::MAX_WORKSPACE_OUTPUT_CAPTURE_BYTES as u64,
            )?;
            let capture = ryeos_state::objects::WorkspaceOutputCapture::from_value(&value)?;
            if capture.partition != context.partition
                || capture.base_project_snapshot_hash != record.base_snapshot
                || capture.result_project_snapshot_hash != *snapshot_hash
                || capture.producer_chain_root_id != context.producer_chain_root_id
                || capture.producer_thread_id != context.producer_thread_id
                || capture.admitted_launch_capsule_hash != context.admitted_launch_capsule_hash
            {
                anyhow::bail!(
                    "frozen workspace capture contradicts its exact producer/generation authority"
                );
            }
        }
        _ => anyhow::bail!("frozen workspace source/output pair is incomplete"),
    }
    Ok(Some(generation))
}

/// Load the exact output half paired with the workspace's current source
/// generation. The previous producer capsule is intentionally not consulted:
/// this object is the retained, owning recovery edge after that capsule may
/// have been collected.
fn load_workspace_output_base_capture(
    authority: &ryeos_state::PinnedStateAuthority,
    record: &ryeos_app::runtime_db::WorkspaceRecord,
    partition: &ryeos_state::objects::WorkspaceOutputPartition,
    policy: &ryeos_state::objects::ProjectSnapshotPolicy,
) -> Result<Option<ryeos_state::objects::WorkspaceOutputCapture>> {
    if record.workspace_output_partition_identity.as_deref()
        != Some(partition.partition_identity.as_str())
    {
        anyhow::bail!("workspace output partition contradicts its durable journal");
    }
    let Some(capture_hash) = record.base_output_capture_hash.as_deref() else {
        return Ok(None);
    };
    let cas = authority.cas_store()?;
    let value = ryeos_state::object_closure::load_exact_cas_object_with_cas(
        &cas,
        capture_hash,
        ryeos_state::objects::MAX_WORKSPACE_OUTPUT_CAPTURE_BYTES as u64,
    )?;
    let capture = ryeos_state::objects::WorkspaceOutputCapture::from_value(&value)?;
    if capture.partition != *partition
        || capture.result_project_snapshot_hash != record.base_snapshot
    {
        anyhow::bail!(
            "workspace output base capture contradicts its source generation or partition"
        );
    }
    let result = ryeos_state::project_materialization::load_project_snapshot_bounded(
        &cas,
        &capture.result_project_snapshot_hash,
    )?
    .ok_or_else(|| anyhow::anyhow!("workspace output capture result snapshot is absent"))?;
    partition.validate_result_source_policy(&result, policy)?;
    Ok(Some(capture))
}

pub(crate) fn fold_back_outputs(params: FoldBackOutputsParams<'_>) -> Result<FoldBackCapture> {
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
        output_partition,
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
    let base_output_capture = if let Some(partition) = output_partition {
        partition.validate()?;
        if partition.project_snapshot_policy_hash != policy_hash {
            anyhow::bail!("workspace output capture lost its admitted source policy");
        }
        for root in &partition.roots {
            for path in pre_tree.files.keys().chain(operational_shadow_paths.iter()) {
                let output = Path::new(&root.path);
                let input = Path::new(path);
                if input.starts_with(output) || output.starts_with(input) {
                    anyhow::bail!("workspace output capture overlaps source/input `{path}`");
                }
            }
        }
        load_workspace_output_base_capture(authority, workspace_record, partition, policy)?
    } else {
        if workspace_record
            .workspace_output_partition_identity
            .is_some()
            || workspace_record.base_output_capture_hash.is_some()
        {
            anyhow::bail!("ordinary fold-back retained workspace output authority");
        }
        None
    };

    let layout = workspace::WorkspaceLayout::from_root(working_dir.to_path_buf());
    let lifecycle = isolation
        .workspace_lifecycle_pinned(ryeos_engine::isolation::WorkspaceLifecycleInvocation {
            operation: ryeos_isolation_protocol::WorkspaceLifecycleOperation::FreezeAndDiff,
            workspace_id,
            launch_owner,
            base_snapshot: base_snapshot_hash,
            project_path: &layout.project,
            mount_identity: workspace_record.mount_identity.as_deref(),
        })
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    let pinned = lillux::canonical_json(&serde_json::to_value(
        &lifecycle.evidence.pinned_root_identities,
    )?)?;
    if workspace_record.workspace_id != workspace_id
        || workspace_record.base_snapshot != base_snapshot_hash
        || workspace_record.launch_owner.as_deref() != Some(launch_owner)
        || workspace_record.backend_id.as_deref() != Some(lifecycle.evidence.backend_id.as_str())
        || workspace_record.backend_version.as_deref()
            != Some(lifecycle.evidence.backend_version.as_str())
        || workspace_record.pinned_root_identities.as_deref() != Some(pinned.as_str())
        || workspace_record.mount_identity != lifecycle.evidence.mount_identity
    {
        anyhow::bail!("workspace freeze evidence does not match the durable creation journal");
    }
    let mut captured_outputs = None;
    let new_tree = if isolation.is_enforced() {
        let mutation_content = lifecycle.mutation_content.as_ref().ok_or_else(|| {
            anyhow::anyhow!("workspace adapter omitted its pinned mutation-content root")
        })?;
        let mut source_exclusions = operational_shadow_paths.to_vec();
        if let Some(partition) = output_partition {
            source_exclusions.extend(partition.roots.iter().map(|root| root.path.clone()));
            source_exclusions.sort();
            source_exclusions.dedup();
            captured_outputs = Some(workspace_outputs::apply_output_delta(
                authority,
                cas_mutation_guard,
                &mut staged_roots,
                mutation_content,
                partition,
                policy,
                base_output_capture.as_ref(),
                &lifecycle.evidence.mutations,
            )?);
        }
        workspace::apply_workspace_delta(
            authority,
            cas_mutation_guard,
            &mut staged_roots,
            mutation_content,
            pre_tree,
            policy,
            &lifecycle.evidence.mutations,
            &source_exclusions,
        )?
    } else {
        let project = lillux::PinnedDirectory::open(&layout.project)?
            .ok_or_else(|| anyhow::anyhow!("daemon-private workspace project disappeared"))?;
        let mut source_exclusions = operational_shadow_paths.to_vec();
        if let Some(partition) = output_partition {
            captured_outputs = Some(workspace_outputs::capture_native_workspace_outputs(
                authority,
                cas_mutation_guard,
                &mut staged_roots,
                &project,
                partition,
                policy,
                isolation,
            )?);
            source_exclusions.extend(partition.roots.iter().map(|root| root.path.clone()));
            source_exclusions.sort();
            source_exclusions.dedup();
        }
        let mut captured = ingest::ingest_project_tree_with_operational_exclusions(
            authority,
            cas_mutation_guard,
            &project,
            policy,
            &source_exclusions,
        )?;
        // Copy-bound inputs shadow project files just as read-only mounts do.
        // Omitting their process-visible bytes must preserve any original
        // project bytes underneath, rather than turning an input overlay into
        // an authored deletion during native fold-back.
        ingest::restore_operational_shadow_files(
            &mut captured,
            pre_tree,
            operational_shadow_paths,
        )?;
        ryeos_state::project_sync::validate_project_tree_paths(&captured, policy)?;
        (captured != *pre_tree).then_some(captured)
    };
    let Some(new_tree) = new_tree else {
        return Ok(FoldBackCapture {
            tree_hash: None,
            outputs: captured_outputs,
            publication: PendingCasPublication::new(authority.try_clone()?, staged_roots),
        });
    };
    let new_hash =
        staged_roots.store_object_admitted(cas_mutation_guard, &cas, &new_tree.to_value())?;

    tracing::debug!(
        old_hash = pre_tree_hash,
        new_hash = %new_hash,
        "fold-back produced new project tree"
    );

    Ok(FoldBackCapture {
        tree_hash: Some(new_hash),
        outputs: captured_outputs,
        publication: PendingCasPublication::new(authority.try_clone()?, staged_roots),
    })
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
    effective_project: &Path,
    base_snapshot_hash: &str,
    _root_contact_fence: &ryeos_app::hosted_operation::HostedRootTerminalizationGuard,
) -> Result<PendingProjectResult> {
    // The caller acquired this exact placement's existing root gate BEFORE
    // taking a capture-work permit. Acquiring it here can deadlock all capture
    // slots while an earlier root operation waits for a slot to finish. A
    // held worker may already possess the view before attachment, so draining
    // that gate cannot be replaced by inspecting only attached process rows.
    // The caller does not commit it: Freezing is the durable contact fence;
    // a follow/continuation capture does not itself terminalize the root.
    let authority = pinned_state_authority(state)?;
    let guard = authority.acquire_shared_guard()?;
    let cas = authority.cas_store()?;
    let snapshot = ryeos_state::project_materialization::load_project_snapshot_bounded(
        &cas,
        base_snapshot_hash,
    )?
    .ok_or_else(|| anyhow::anyhow!("base project snapshot {base_snapshot_hash} is absent"))?;
    let workspace = workspace::WorkspaceLayout::from_project(effective_project)?;
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
    if record.base_snapshot != base_snapshot_hash {
        anyhow::bail!("callback workspace base snapshot contradicts its resume base");
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
    let quiesced = quiesce_bound_workspace(state, &record)?;
    let (operational_shadow_paths, output_context) =
        admitted_workspace_capture_inputs(state, thread_id, &record)?;
    if let Some(generation) =
        verified_frozen_generation(&authority, &record, output_context.as_ref())?
    {
        return Ok(PendingProjectResult {
            generation,
            publication: None,
            quiesced: Some(quiesced),
        });
    }
    let permit = state
        .write_barrier
        .acquire_with_timeout(ryeos_app::write_barrier::ONLINE_WRITE_PERMIT_TIMEOUT)
        .map_err(|error| anyhow::anyhow!("acquire callback generation write permit: {error}"))?;
    let FoldBackCapture {
        tree_hash: next_tree,
        outputs,
        mut publication,
    } = fold_back_outputs(FoldBackOutputsParams {
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
        output_partition: output_context.as_ref().map(|context| &context.partition),
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
    let generation = ryeos_state::objects::WorkspaceGenerationPair {
        output_capture_hash: store_workspace_output_capture(
            &authority,
            &guard,
            output_context.as_ref(),
            outputs,
            base_snapshot_hash,
            &snapshot_hash,
            &mut publication,
        )?,
        snapshot_hash,
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
        &generation,
    )?;
    Ok(PendingProjectResult {
        generation,
        publication: Some(publication),
        quiesced: Some(quiesced),
    })
}

/// Capture the exact current COW generation for a workload-delegated
/// immutable child without entering the workspace's one-way candidate-freeze
/// lifecycle. `RuntimeActionIntent` owns the durable barrier and selected
/// input; `execution_workspace` remains only the materialization journal.
pub(crate) fn capture_runtime_workspace_input_generation(
    state: &ryeos_app::state::AppState,
    operation_id: &str,
    thread_id: &str,
    effective_project: &Path,
    base_snapshot_hash: &str,
) -> Result<PendingProjectResult> {
    let intent = state
        .state_store
        .get_runtime_action_intent(operation_id)?
        .ok_or_else(|| anyhow::anyhow!("runtime workspace-operation intent is absent"))?;
    let operation = intent
        .workspace_operation
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("runtime action has no workspace-operation authority"))?;
    if operation.access != ryeos_engine::kind_registry::WorkspaceAccess::ImmutableCurrentGeneration
        || operation.phase != ryeos_app::runtime_db::RuntimeWorkspaceOperationPhase::Reserved
        || intent.first_caller_thread_id != thread_id
    {
        anyhow::bail!("runtime workspace operation is not an unstarted immutable capture");
    }

    let workspace = workspace::WorkspaceLayout::from_project(effective_project)?;
    let workspace_id = workspace
        .root
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| anyhow::anyhow!("workspace id is not valid UTF-8"))?;
    if operation.workspace_id != workspace_id {
        anyhow::bail!("runtime workspace operation names a different execution workspace");
    }
    let record = state
        .state_store
        .execution_workspace(workspace_id)?
        .ok_or_else(|| anyhow::anyhow!("workspace journal row is missing"))?;
    let launch_owner = record
        .launch_owner
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("workspace has no launch owner"))?;
    if record.state != WorkspaceState::Active
        || record.thread_id.as_deref() != Some(thread_id)
        || record.base_snapshot != base_snapshot_hash
    {
        anyhow::bail!("runtime workspace capture contradicts its active workspace journal");
    }
    state
        .state_store
        .assert_launch_owner(thread_id, launch_owner)?;
    state.state_store.transition_runtime_workspace_operation(
        operation_id,
        &[ryeos_app::runtime_db::RuntimeWorkspaceOperationPhase::Reserved],
        ryeos_app::runtime_db::RuntimeWorkspaceOperationPhase::Quiescing,
    )?;
    let quiesced = quiesce_bound_workspace(state, &record)?;

    let capture = (|| -> Result<(ryeos_state::objects::WorkspaceGenerationPair, PendingCasPublication)> {
        let authority = pinned_state_authority(state)?;
        let guard = authority.acquire_shared_guard()?;
        let cas = authority.cas_store()?;
        let snapshot = ryeos_state::project_materialization::load_project_snapshot_bounded(
            &cas,
            base_snapshot_hash,
        )?
        .ok_or_else(|| anyhow::anyhow!("base project snapshot {base_snapshot_hash} is absent"))?;
        let permit = state
            .write_barrier
            .acquire_with_timeout(ryeos_app::write_barrier::ONLINE_WRITE_PERMIT_TIMEOUT)
            .map_err(|error| anyhow::anyhow!("acquire workspace-input write permit: {error}"))?;
        let (operational_shadow_paths, output_context) =
            admitted_workspace_capture_inputs(state, thread_id, &record)?;
        let FoldBackCapture { tree_hash: next_tree, outputs, mut publication } = fold_back_outputs(FoldBackOutputsParams {
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
            output_partition: output_context.as_ref().map(|context| &context.partition),
        })?;
        let snapshot_hash = match next_tree {
            Some(tree_hash) => store_foldback_snapshot(
                &authority,
                &guard,
                &tree_hash,
                base_snapshot_hash,
                &mut publication,
            )?,
            None => base_snapshot_hash.to_owned(),
        };
        let generation = ryeos_state::objects::WorkspaceGenerationPair {
            output_capture_hash: store_workspace_output_capture(
                &authority, &guard, output_context.as_ref(), outputs,
                base_snapshot_hash, &snapshot_hash, &mut publication,
            )?,
            snapshot_hash,
        };
        drop(permit);
        state
            .state_store
            .assert_launch_owner(thread_id, launch_owner)?;
        state
            .state_store
            .bind_runtime_workspace_input_generation(operation_id, &generation)?;
        Ok((generation, publication))
    })();

    match capture {
        Ok((generation, publication)) => Ok(PendingProjectResult {
            generation,
            publication: Some(publication),
            quiesced: Some(quiesced),
        }),
        Err(error) => match quiesced.resume_or_terminate() {
            Ok(()) => {
                state.state_store.transition_runtime_workspace_operation(
                    operation_id,
                    &[ryeos_app::runtime_db::RuntimeWorkspaceOperationPhase::Quiescing],
                    ryeos_app::runtime_db::RuntimeWorkspaceOperationPhase::Released,
                )?;
                Err(error)
            }
            Err(settle_error) => Err(error.context(format!(
                "workspace-input capture failed and exact borrower resume/termination was not proved: {settle_error:#}"
            ))),
        },
    }
}

/// Quiesce every exact view borrower while a workload-delegated child receives
/// exclusive access to its existing mutable CoW workspace.
///
/// The durable barrier and phase live on the existing `RuntimeActionIntent`;
/// `execution_workspace` remains only the materialization/candidate journal.
/// Do not add an exclusive-operation table or reuse the one-way `Freezing`
/// state for this transient operation.
pub(crate) fn quiesce_runtime_workspace_exclusive(
    state: &ryeos_app::state::AppState,
    operation_id: &str,
    thread_id: &str,
    effective_project: &Path,
    base_snapshot_hash: &str,
) -> Result<ExclusiveWorkspaceQuiescence> {
    let intent = state
        .state_store
        .get_runtime_action_intent(operation_id)?
        .ok_or_else(|| anyhow::anyhow!("runtime workspace-operation intent is absent"))?;
    let operation = intent
        .workspace_operation
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("runtime action has no workspace-operation authority"))?;
    if operation.access != ryeos_engine::kind_registry::WorkspaceAccess::SharedExclusive
        || operation.phase != ryeos_app::runtime_db::RuntimeWorkspaceOperationPhase::Reserved
        || intent.first_caller_thread_id != thread_id
    {
        anyhow::bail!("runtime workspace operation is not an unstarted exclusive operation");
    }

    let workspace = workspace::WorkspaceLayout::from_project(effective_project)?;
    let workspace_id = workspace
        .root
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| anyhow::anyhow!("workspace id is not valid UTF-8"))?;
    if operation.workspace_id != workspace_id {
        anyhow::bail!("runtime workspace operation names a different execution workspace");
    }
    let record = state
        .state_store
        .execution_workspace(workspace_id)?
        .ok_or_else(|| anyhow::anyhow!("workspace journal row is missing"))?;
    let launch_owner = record
        .launch_owner
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("workspace has no launch owner"))?;
    if record.state != WorkspaceState::Active
        || record.thread_id.as_deref() != Some(thread_id)
        || record.base_snapshot != base_snapshot_hash
    {
        anyhow::bail!("exclusive workspace operation contradicts its active workspace journal");
    }
    state
        .state_store
        .assert_launch_owner(thread_id, launch_owner)?;
    state.state_store.transition_runtime_workspace_operation(
        operation_id,
        &[ryeos_app::runtime_db::RuntimeWorkspaceOperationPhase::Reserved],
        ryeos_app::runtime_db::RuntimeWorkspaceOperationPhase::Quiescing,
    )?;
    let quiesced = quiesce_bound_workspace(state, &record)?;
    if let Err(error) = state.state_store.transition_runtime_workspace_operation(
        operation_id,
        &[ryeos_app::runtime_db::RuntimeWorkspaceOperationPhase::Quiescing],
        ryeos_app::runtime_db::RuntimeWorkspaceOperationPhase::Quiesced,
    ) {
        return match quiesced.resume_or_terminate() {
            Ok(()) => {
                state.state_store.transition_runtime_workspace_operation(
                    operation_id,
                    &[ryeos_app::runtime_db::RuntimeWorkspaceOperationPhase::Quiescing],
                    ryeos_app::runtime_db::RuntimeWorkspaceOperationPhase::Released,
                )?;
                Err(error.context("record exact exclusive workspace quiescence"))
            }
            Err(settle_error) => Err(error.context(format!(
                "exclusive quiescence could not be recorded and exact borrower resume/termination was not proved: {settle_error:#}"
            ))),
        };
    }
    Ok(ExclusiveWorkspaceQuiescence {
        group: Some(quiesced),
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
    // Drain root-owned contacts before occupying a scarce capture slot. Move
    // the guard into the blocking closure so cancellation cannot release the
    // fence while filesystem capture is still running.
    let root_contact_fence = ryeos_app::hosted_operation::begin_hosted_root_terminalization_async(
        &state.state_store,
        &thread_id,
    )
    .await?;
    let pending = run_bounded_project_capture(move || {
        seal_callback_workspace_generation(
            &capture_state,
            &capture_thread_id,
            &effective_path,
            &base_snapshot_hash,
            &root_contact_fence,
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
) -> Result<Option<ryeos_state::objects::WorkspaceGenerationPair>> {
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
    // The later terminal-state commit acquires its own guard. This temporary
    // disposition fence drains worker starts before any post-exit capture;
    // missing attachment metadata alone is not a no-contact proof.
    let _root_contact_fence = ryeos_app::hosted_operation::begin_hosted_root_terminalization(
        &state.state_store,
        thread_id,
    )?;
    state
        .state_store
        .assert_execution_process_detached_owned(thread_id, launch_owner)?;

    let layout = workspace::WorkspaceLayout::from_project(provenance.effective_path())?;
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
    let generation = recover_interrupted_workspace_freeze(state, &record)?;
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
            &generation.snapshot_hash,
        )?;
    }
    Ok(Some(generation))
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
    state.state_store.advance_project_head_ref_owned(
        thread_id,
        launch_owner,
        principal_key,
        project_hash,
        result_snapshot_hash,
        expected_hash,
        &signer,
        &cas_guard,
    )
}

/// Complete a write-ahead callback freeze whose runtime owner died after the
/// workspace entered `freezing` but before its snapshot binding committed.
/// The dead process makes the backend-owned mutation state stable; the exact
/// captured adapter replays FreezeAndDiff against the preserved journal
/// identity.
pub fn recover_interrupted_workspace_freeze(
    state: &ryeos_app::state::AppState,
    record: &ryeos_app::runtime_db::WorkspaceRecord,
) -> Result<ryeos_state::objects::WorkspaceGenerationPair> {
    recover_interrupted_workspace_freeze_inner(state, record, false)
}

/// Startup recovery for the same journal after the prior daemon's launch
/// claim has already been cleared. The StateStore retains a distinct,
/// dead-generation-only bind fence; ordinary live freeze completion continues
/// to require the active launch claim.
pub fn recover_abandoned_interrupted_workspace_freeze(
    state: &ryeos_app::state::AppState,
    record: &ryeos_app::runtime_db::WorkspaceRecord,
) -> Result<ryeos_state::objects::WorkspaceGenerationPair> {
    recover_interrupted_workspace_freeze_inner(state, record, true)
}

fn recover_interrupted_workspace_freeze_inner(
    state: &ryeos_app::state::AppState,
    record: &ryeos_app::runtime_db::WorkspaceRecord,
    abandoned_owner: bool,
) -> Result<ryeos_state::objects::WorkspaceGenerationPair> {
    if record.state != WorkspaceState::Freezing {
        anyhow::bail!("only a freezing workspace can recover a callback generation");
    }
    let thread_id = record
        .thread_id
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("freezing workspace has no thread owner"))?;
    let launch_owner = record
        .launch_owner
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("freezing workspace has no launch owner"))?;
    assert_workspace_capture_processes_settled(state, record)?;
    let authority = pinned_state_authority(state)?;
    let guard = authority.acquire_shared_guard()?;
    let (operational_shadow_paths, output_context) =
        admitted_workspace_capture_inputs(state, thread_id, record)?;
    if let Some(generation) =
        verified_frozen_generation(&authority, record, output_context.as_ref())?
    {
        return Ok(generation);
    }
    let cas = authority.cas_store()?;
    let base = ryeos_state::project_materialization::load_project_snapshot_bounded(
        &cas,
        &record.base_snapshot,
    )?
    .ok_or_else(|| anyhow::anyhow!("freezing workspace base snapshot is absent"))?;
    let permit = state
        .write_barrier
        .acquire_with_timeout(ryeos_app::write_barrier::ONLINE_WRITE_PERMIT_TIMEOUT)
        .map_err(|error| anyhow::anyhow!("acquire recovery freeze write permit: {error}"))?;
    let FoldBackCapture {
        tree_hash: next_tree,
        outputs,
        mut publication,
    } = fold_back_outputs(FoldBackOutputsParams {
        authority: &authority,
        cas_mutation_guard: &guard,
        isolation: &state.isolation,
        workspace_id: &record.workspace_id,
        launch_owner,
        working_dir: Path::new(&record.root_path),
        pre_tree_hash: &base.project_tree_hash,
        policy_hash: &base.effective_policy_hash,
        base_snapshot_hash: &record.base_snapshot,
        workspace_record: record,
        operational_shadow_paths: &operational_shadow_paths,
        output_partition: output_context.as_ref().map(|context| &context.partition),
    })?;
    let snapshot_hash = match next_tree {
        Some(tree_hash) => store_foldback_snapshot(
            &authority,
            &guard,
            &tree_hash,
            &record.base_snapshot,
            &mut publication,
        )?,
        None => record.base_snapshot.clone(),
    };
    let generation = ryeos_state::objects::WorkspaceGenerationPair {
        output_capture_hash: store_workspace_output_capture(
            &authority,
            &guard,
            output_context.as_ref(),
            outputs,
            &record.base_snapshot,
            &snapshot_hash,
            &mut publication,
        )?,
        snapshot_hash,
    };
    drop(permit);
    if abandoned_owner {
        state
            .state_store
            .bind_abandoned_frozen_execution_workspace(
                &record.workspace_id,
                thread_id,
                launch_owner,
                &generation,
            )?;
    } else {
        state.state_store.bind_frozen_execution_workspace(
            &record.workspace_id,
            thread_id,
            launch_owner,
            &generation,
        )?;
    }
    publication.publish()?;
    Ok(generation)
}

/// Stop every current borrower under the caller's existing admission barrier.
/// RuntimeActionIntent fences transient input/exclusive operations; a callback
/// first enters Freezing. Neither barrier permits a new same-view admission.
/// The caller must also drain any already-started worker contact through the
/// existing root operation owner before a one-way callback/terminal freeze.
///
/// This is an invocation-local set of retained Lillux stop authorities, not a
/// new borrower registry. An indexed member with no attached exact process is
/// unfinished contact, never an ignorable idle thread. Any refusal drops and
/// resumes all groups already stopped during this acquisition.
fn quiesce_bound_workspace(
    state: &ryeos_app::state::AppState,
    record: &ryeos_app::runtime_db::WorkspaceRecord,
) -> Result<QuiescedExecutionGroup> {
    let view_identity = record
        .mount_identity
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("workspace capture has no created view identity"))?;
    let root = record
        .thread_id
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("workspace capture has no root owner"))?;
    let launch_owner = record
        .launch_owner
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("workspace capture has no exact launch owner"))?;
    // The existing pool/durable session owners also cover pre-attachment
    // starts and failed cleanup that cannot be inferred from the journal PID.
    // Resolve that readiness before stopping any process group.
    let worker_identity =
        ryeos_app::dedicated_session_service::workspace_worker_capture_identity(state, record)?;
    let workspace_identity: Option<ryeos_app::process::ExecutionProcessIdentity> = record
        .process_identity
        .as_deref()
        .map(serde_json::from_str)
        .transpose()
        .context("decode workspace process owner")?;
    let mut quiesced = QuiescedExecutionGroup {
        authorities: Vec::new(),
    };
    let mut groups = std::collections::BTreeMap::new();
    let root_identity = state
        .state_store
        .execution_process_identity_owned(root, launch_owner)?;
    quiesced.stop_once(&root_identity, &mut groups)?;
    let mut after = None;
    let mut root_seen = false;
    loop {
        let members = state.state_store.workspace_members_for_recovery_after(
            &record.workspace_id,
            after.as_deref(),
            ryeos_app::runtime_db::WORKSPACE_MEMBER_PAGE_SIZE,
        )?;
        if members.is_empty() {
            break;
        }
        for member in &members {
            if member.binding.view_identity != view_identity {
                anyhow::bail!("workspace capture retains an unresolved prior view incarnation");
            }
            let owner = lillux::canonical_json(&serde_json::to_value(
                &member.binding.borrower_launch_owner,
            )?)?;
            if member.thread_id == root {
                if owner != launch_owner {
                    anyhow::bail!("workspace capture root membership changed launch owner");
                }
                root_seen = true;
            }
            let identity = state
                .state_store
                .execution_process_identity_owned(&member.thread_id, &owner)
                .with_context(|| {
                    format!(
                        "workspace member {} has unresolved process contact",
                        member.thread_id
                    )
                })?;
            quiesced.stop_once(&identity, &mut groups)?;
        }
        after = members.last().map(|member| member.thread_id.clone());
    }
    if !root_seen {
        anyhow::bail!("live workspace capture has no exact root view membership");
    }
    // An exclusive worker has its own process owner, while the placement
    // thread's runtime identity names its controller. Stop both; stopping the
    // controller alone does not stabilize the shared upper tree.
    for identity in worker_identity.iter().chain(workspace_identity.iter()) {
        match ryeos_app::process::execution_liveness(identity) {
            ryeos_app::process::IdentityLiveness::DeadOrStale => {
                // A callback may freeze after an exclusive worker has been
                // retired. Its exact retained identity still requires whole
                // group absence; a dead leader alone does not stabilize it.
                ryeos_app::process::assert_reaped_process_group_absent(identity)?;
            }
            _ => quiesced.stop_once(identity, &mut groups)?,
        }
    }
    Ok(quiesced)
}

/// The caller already owns the root contact fence. Empty membership is the
/// result of exact process/contact settlement, not an inference from a missing
/// PID. Retained workspace identity separately covers a dedicated worker.
pub(crate) fn assert_workspace_capture_processes_settled(
    state: &ryeos_app::state::AppState,
    record: &ryeos_app::runtime_db::WorkspaceRecord,
) -> Result<()> {
    let worker_identity =
        ryeos_app::dedicated_session_service::workspace_worker_capture_identity(state, record)?;
    if state
        .state_store
        .execution_workspace_has_members(&record.workspace_id)?
    {
        anyhow::bail!("terminal workspace capture retains unresolved view members");
    }
    let root = record
        .thread_id
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("terminal workspace capture has no root owner"))?;
    let thread = state
        .state_store
        .get_thread(root)?
        .ok_or_else(|| anyhow::anyhow!("terminal workspace capture root disappeared"))?;
    if let Some(identity) = thread.runtime.process_identity.as_ref() {
        ryeos_app::process::assert_reaped_process_group_absent(identity)?;
    } else if thread.runtime.pid.is_some() || thread.runtime.pgid.is_some() {
        anyhow::bail!("terminal workspace capture root has incomplete process identity");
    }
    if let Some(encoded) = record.process_identity.as_deref() {
        let identity = serde_json::from_str(encoded).context("decode workspace process owner")?;
        ryeos_app::process::assert_reaped_process_group_absent(&identity)?;
    }
    if let Some(identity) = worker_identity.as_ref() {
        ryeos_app::process::assert_reaped_process_group_absent(identity)?;
    }
    Ok(())
}

/// One capture guard can cover multiple process groups borrowing the same
/// created view. The individual Lillux guards remain the signal/death owners.
pub(crate) struct QuiescedExecutionGroup {
    authorities: Vec<lillux::QuiescedProcesses>,
}

/// Local capture inventory only, never another durable process registry.
/// Whole scopes may contain several groups; deduplicating them by PGID would
/// either omit a scope or try to acquire its freeze barrier twice.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum WorkspaceCaptureOwner {
    Group(i64),
    Scope(String),
}

/// A group or scope may appear as both membership and workspace owner. Scope
/// identity is the complete Lillux token, not its original target/group PID.
fn register_workspace_capture_group(
    groups: &mut std::collections::BTreeMap<WorkspaceCaptureOwner, Option<(String, i64)>>,
    identity: &ryeos_app::process::ExecutionProcessIdentity,
) -> Result<bool> {
    ryeos_app::process::validate_execution_process_identity_shape(identity)?;
    if let Some(scope) = &identity.process_scope {
        let key =
            WorkspaceCaptureOwner::Scope(lillux::canonical_json(&serde_json::to_value(scope)?)?);
        return Ok(groups.insert(key, None).is_none());
    }
    let key = WorkspaceCaptureOwner::Group(identity.group_leader_pid);
    let incarnation = (&identity.boot_id, identity.group_leader_start_time_ticks);
    if let Some(Some((boot, birth))) = groups.get(&key) {
        if (boot, *birth) != incarnation {
            anyhow::bail!("workspace members name conflicting process-group incarnations");
        }
        return Ok(false);
    }
    groups.insert(
        key,
        Some((
            identity.boot_id.clone(),
            identity.group_leader_start_time_ticks,
        )),
    );
    Ok(true)
}

impl QuiescedExecutionGroup {
    fn stop_once(
        &mut self,
        identity: &ryeos_app::process::ExecutionProcessIdentity,
        groups: &mut std::collections::BTreeMap<WorkspaceCaptureOwner, Option<(String, i64)>>,
    ) -> Result<()> {
        let first = register_workspace_capture_group(groups, identity)?;
        // Even another target in an already-stopped group must still match
        // its exact recorded incarnation. Never waive stale member evidence
        // merely because another member happens to share its numeric PGID.
        if ryeos_app::process::execution_liveness(identity)
            != ryeos_app::process::IdentityLiveness::Alive
        {
            anyhow::bail!("workspace member exact process liveness is not proved");
        }
        if first {
            self.authorities
                .push(ryeos_app::process::quiesce_exact_process_group(
                    identity,
                    lillux::time::Duration::from_secs(2),
                )?);
        }
        Ok(())
    }

    pub(crate) fn resume_or_terminate(mut self) -> Result<()> {
        let mut failures = Vec::new();
        for authority in self.authorities.drain(..) {
            if let Err(error) = authority.resume_or_terminate(lillux::time::Duration::from_secs(5))
            {
                failures.push(error);
            }
        }
        if !failures.is_empty() {
            anyhow::bail!(
                "workspace group resume/termination failed: {}",
                failures.join("; ")
            );
        }
        Ok(())
    }

    pub(crate) fn terminate(mut self) -> Result<()> {
        let mut failures = Vec::new();
        for authority in self.authorities.drain(..) {
            if let Err(error) = authority.terminate(lillux::time::Duration::from_secs(5)) {
                failures.push(error);
            }
        }
        if !failures.is_empty() {
            anyhow::bail!(
                "workspace group termination failed: {}",
                failures.join("; ")
            );
        }
        Ok(())
    }
}

/// Cancellation-safe ownership of exclusively quiesced workspace borrowers.
///
/// An ordinary capture guard resumes on drop. Exclusive workspace execution
/// cannot do that: if its async owner is cancelled while the durable intent
/// still names a running child, resuming would allow two writers. This wrapper
/// therefore proves termination with the retained pidfds unless the normal
/// settlement path explicitly resumes first.
pub(crate) struct ExclusiveWorkspaceQuiescence {
    group: Option<QuiescedExecutionGroup>,
}

impl ExclusiveWorkspaceQuiescence {
    pub(crate) fn resume_or_terminate(mut self) -> Result<()> {
        self.group
            .take()
            .ok_or_else(|| anyhow::anyhow!("exclusive workspace quiescence is absent"))?
            .resume_or_terminate()
    }
}

impl Drop for ExclusiveWorkspaceQuiescence {
    fn drop(&mut self) {
        let Some(group) = self.group.take() else {
            return;
        };
        if let Err(error) = group.terminate() {
            tracing::error!(
                %error,
                "failed to terminate exclusively quiesced workspace execution groups"
            );
        }
    }
}

impl Drop for QuiescedExecutionGroup {
    fn drop(&mut self) {
        for authority in self.authorities.drain(..) {
            if let Err(error) = authority.resume(lillux::time::Duration::from_secs(5)) {
                tracing::error!(%error, "failed to resume an exact quiesced execution group");
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

    #[test]
    fn workspace_capture_group_deduplication_requires_exact_group_birth() {
        let identity = ryeos_app::process::ExecutionProcessIdentity {
            schema_version: ryeos_app::process::PROCESS_IDENTITY_SCHEMA_VERSION,
            process_scope: None,
            boot_id: "fixture-boot".to_owned(),
            target_pid: 40,
            target_start_time_ticks: 200,
            group_leader_pid: 39,
            group_leader_start_time_ticks: 190,
            resource_selections: Vec::new(),
            resource_operations: Vec::new(),
            resource_allocation_limit: None,
            resource_occupancy_start: None,
            resource_occupancy_limit: None,
            resource_cleanup_allowance_ms: None,
        };
        let mut groups = BTreeMap::new();
        assert!(register_workspace_capture_group(&mut groups, &identity).unwrap());
        assert!(!register_workspace_capture_group(&mut groups, &identity).unwrap());
        let mut same_group_target = identity.clone();
        same_group_target.target_pid = 41;
        same_group_target.target_start_time_ticks = 201;
        assert!(!register_workspace_capture_group(&mut groups, &same_group_target).unwrap());

        let mut reused_group = identity.clone();
        reused_group.group_leader_start_time_ticks += 1;
        assert!(register_workspace_capture_group(&mut groups, &reused_group).is_err());
        let mut other_boot = identity.clone();
        other_boot.boot_id = "another-boot".to_owned();
        assert!(register_workspace_capture_group(&mut groups, &other_boot).is_err());
        assert_eq!(groups.len(), 1);

        let mut separate_group = identity;
        separate_group.group_leader_pid = 49;
        assert!(register_workspace_capture_group(&mut groups, &separate_group).unwrap());
        assert_eq!(groups.len(), 2);
    }

    #[test]
    fn workspace_capture_deduplicates_scope_across_distinct_process_groups() {
        let boot = "00000000-0000-4000-8000-000000000000";
        let scope: lillux::ProcessScopeRecovery = serde_json::from_value(serde_json::json!({
            "version": 4, "control_timeout": {"secs": 1, "nanos": 0}, "configuration": {"version": 3, "backend": {
                "implementation": "linux_cgroup_v2", "parent": "/fixture/delegation"
            }},
            "backend": {"implementation": "linux_cgroup_v2", "boot_id": boot,
                "parent": {"containing_device": 1, "inode": 2},
                "directory": {"containing_device": 1, "inode": 3}, "name": "fixture-scope"}
        }))
        .unwrap();
        let mut identity = ryeos_app::process::ExecutionProcessIdentity {
            schema_version: ryeos_app::process::PROCESS_IDENTITY_SCHEMA_VERSION,
            process_scope: Some(scope),
            boot_id: boot.to_owned(),
            target_pid: 40,
            target_start_time_ticks: 200,
            group_leader_pid: 39,
            group_leader_start_time_ticks: 190,
            resource_selections: Vec::new(),
            resource_operations: Vec::new(),
            resource_allocation_limit: None,
            resource_occupancy_start: None,
            resource_occupancy_limit: None,
            resource_cleanup_allowance_ms: None,
        };
        let mut groups = BTreeMap::new();
        assert!(register_workspace_capture_group(&mut groups, &identity).unwrap());
        identity.target_pid = 51;
        identity.target_start_time_ticks = 301;
        identity.group_leader_pid = 50;
        identity.group_leader_start_time_ticks = 300;
        assert!(!register_workspace_capture_group(&mut groups, &identity).unwrap());
        assert_eq!(groups.len(), 1);
        // A strict group is a different control owner, even with the same
        // numeric leader as a target retained inside an admitted scope.
        identity.process_scope = None;
        assert!(register_workspace_capture_group(&mut groups, &identity).unwrap());
        assert_eq!(groups.len(), 2);
    }

    #[test]
    fn workspace_capture_group_inventory_refuses_incomplete_identity() {
        let identity = ryeos_app::process::ExecutionProcessIdentity {
            schema_version: ryeos_app::process::PROCESS_IDENTITY_SCHEMA_VERSION,
            process_scope: None,
            boot_id: "fixture-boot".to_owned(),
            target_pid: 40,
            target_start_time_ticks: 200,
            group_leader_pid: 39,
            group_leader_start_time_ticks: 0,
            resource_selections: Vec::new(),
            resource_operations: Vec::new(),
            resource_allocation_limit: None,
            resource_occupancy_start: None,
            resource_occupancy_limit: None,
            resource_cleanup_allowance_ms: None,
        };
        let mut groups = BTreeMap::new();
        assert!(register_workspace_capture_group(&mut groups, &identity).is_err());
        assert!(groups.is_empty());
    }

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
            &ryeos_state::ignore::IgnoreMatcher::from_config(&ryeos_state::ignore::IgnoreConfig {
                patterns: Vec::new(),
            })
            .unwrap(),
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

        let (shared_path, _shared_lease, shared) = checkout_project_snapshot(
            &authority,
            &guard,
            &snapshot_hash,
            ProjectMaterialization::SharedReadOnly,
            &cache,
        )
        .unwrap();
        let first_root = tempfile::tempdir().unwrap();
        let second_root = tempfile::tempdir().unwrap();
        let first_budget = external_content::PrivateMaterializationBudget::new(1024);
        let second_budget = external_content::PrivateMaterializationBudget::new(1024);
        let (first_path, _first_lease, _first) = checkout_project_snapshot(
            &authority,
            &guard,
            &snapshot_hash,
            ProjectMaterialization::PrivateWritableWorkspace {
                target_dir: first_root.path(),
                budget: Some(&first_budget),
            },
            &cache,
        )
        .unwrap();
        let (second_path, _second_lease, second) = checkout_project_snapshot(
            &authority,
            &guard,
            &snapshot_hash,
            ProjectMaterialization::PrivateWritableWorkspace {
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
            LiveFilesystemConfinement::standard_fixed_parents(),
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

    #[test]
    fn pin_at_spawn_can_retain_a_private_child_result_without_head_authority() {
        let root = tempfile::tempdir().unwrap();
        let parent = ExecutionProjectAuthority::live(
            root.path().canonicalize().unwrap(),
            "project:test".to_string(),
            LiveProjectAccess::ReadWrite,
            LiveFilesystemConfinement::standard_fixed_parents(),
            EnvironmentAuthority::None,
            Vec::new(),
        )
        .unwrap();

        let child = derive_pinned_child_authority(
            &parent,
            "a".repeat(64),
            PinnedChildProjectRealization::CowRetainResult,
        )
        .unwrap();
        let ExecutionProjectAuthority::PinnedGeneration {
            realization:
                ryeos_state::objects::PinnedProjectRealization::Cow {
                    terminal_publication,
                },
            ..
        } = child
        else {
            panic!("retained child policy must produce a private COW authority");
        };
        assert_eq!(
            terminal_publication,
            ryeos_state::objects::PinnedTerminalPublication::RetainResult
        );
    }
}
