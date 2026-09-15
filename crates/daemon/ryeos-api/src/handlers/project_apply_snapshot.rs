//! `project/apply-snapshot` — apply an AI-only snapshot to a live project.

use std::collections::{BTreeSet, HashMap};
use std::ffi::{OsStr, OsString};
#[cfg(test)]
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use anyhow::{Context, Result, anyhow};
use hmac::{Hmac, Mac};
use lillux::cas::CasStore;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::Sha256;

use crate::handler_context::HandlerContext;
use crate::project_deploy::{self, ProjectDeployContext};
use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;
use ryeos_state::objects::{ProjectFile, ProjectSnapshot, ProjectSnapshotPolicy, ProjectTree};
use ryeos_state::project_sync::ProjectSyncScope;
use ryeos_state::{
    FinishSyncJobAttempt, NewSyncJob, NewSyncJobAttempt, SyncJobAttemptState, SyncJobRecord,
    SyncJobState, SyncJobUpdate,
};

#[derive(serde::Deserialize)]
#[serde(transparent)]
pub struct ExplicitExpectedDeployedHash(Option<String>);

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub project_path: String,
    pub snapshot_hash: String,
    /// Exact deployed HEAD observed before the request. Mandatory and nullable.
    pub expected_deployed_hash: ExplicitExpectedDeployedHash,
}

const APPLY_OPERATION_TYPE: &str = "project_snapshot_apply";
const APPLY_OPERATION_SCHEMA: &str = "ryeos.project-snapshot-apply-operation.v1";
const APPLY_JOURNAL_KIND: &str = "ryeos.project-snapshot-apply-journal";
const APPLY_JOURNAL_SCHEMA: u32 = 1;
const APPLY_JOURNAL_MAX_BYTES: u64 = 8 * 1024 * 1024;
const SURFACE_CONTENT_SCHEMA: u32 = 1;

fn deserialize_required_nullable<'de, D, T>(
    deserializer: D,
) -> std::result::Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ApplyOperation {
    operation_type: String,
    schema: String,
    job_id: String,
    attempt_id: String,
    transaction_id: String,
    project_path: String,
    project_hash: String,
    project_identity: lillux::PinnedDirectoryIdentity,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    previous_deployed_hash: Option<String>,
    target_snapshot_hash: String,
    caller_fingerprint: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct JournalSurface {
    relative: String,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    prior_content: Option<SurfaceContentIdentity>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    target_content: Option<SurfaceContentIdentity>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SurfaceContentIdentity {
    schema: u32,
    digest: String,
    regular_files: u64,
    directories: u64,
    total_bytes: u64,
}

#[derive(Debug, Serialize)]
struct SurfaceContentFile {
    path: String,
    blob_hash: String,
    size: u64,
    normalized_mode: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ApplyJournal {
    kind: String,
    schema: u32,
    operation: ApplyOperation,
    surfaces: Vec<JournalSurface>,
    schedule_before_images: Vec<crate::project_deploy::schedules::ScheduleRecoveryBeforeImage>,
    auth_tag: String,
}

impl ApplyJournal {
    fn authenticated_value(&self) -> Result<Value> {
        let mut value = serde_json::to_value(self)?;
        value
            .as_object_mut()
            .context("project apply journal is not an object")?
            .insert("auth_tag".to_owned(), Value::String(String::new()));
        Ok(value)
    }
}

pub async fn handle(req: Request, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    ctx.require_verified().map_err(|e| anyhow!(e))?;

    // Entry log: proves the request reached this handler (vs. being absorbed
    // by a dispatcher 404 / wrong route) and records who is applying what.
    tracing::info!(
        project_path = %req.project_path,
        snapshot_hash = %req.snapshot_hash,
        caller = %ctx.fingerprint,
        "project.apply-snapshot handler entered"
    );

    let project_path = canonical_existing_project_path(&req.project_path)?;
    let canonical_project_path = project_path
        .to_str()
        .ok_or_else(|| anyhow!("canonical project_path is not valid UTF-8"))?
        .to_owned();
    let project_directory = lillux::PinnedDirectory::open(&project_path)?
        .ok_or_else(|| anyhow!("canonical project directory disappeared"))?;
    project_directory.ensure_path_binding()?;
    let project_hash = ryeos_state::refs::deployed_project_key(&canonical_project_path);
    let apply_lock = project_apply_lock(&project_hash);
    let _apply_guard = apply_lock.lock_owned().await;
    let _project_mutation_guard =
        crate::project_namespace::acquire_project_mutation_lock(&project_directory).await?;
    ensure_no_active_apply_job_for_project(&state, &project_hash)?;
    let authority = state
        .state_store
        .with_state_db(|db| db.pinned_authority())?;
    let cas_read_guard = authority.acquire_shared_guard()?;
    let cas = authority.cas_store()?;

    let principal_key = ryeos_state::refs::principal_storage_key(&ctx.fingerprint)?;
    let pushed_head = state
        .state_store
        .with_state_db(|db| db.read_project_head(principal_key, &project_hash))?;
    if pushed_head.as_deref() != Some(req.snapshot_hash.as_str()) {
        anyhow::bail!(
            "project.apply-snapshot refused: caller's staged HEAD for project '{}' is {:?}, not requested snapshot {}",
            canonical_project_path,
            pushed_head,
            req.snapshot_hash
        );
    }

    let snapshot_obj = ryeos_state::object_closure::load_exact_cas_object_with_cas(
        &cas,
        &req.snapshot_hash,
        ryeos_state::object_closure::ObjectClosureLimits::default().max_object_bytes,
    )?;
    let snapshot = ProjectSnapshot::from_value(&snapshot_obj)?;
    let policy_obj = ryeos_state::object_closure::load_exact_cas_object_with_cas(
        &cas,
        &snapshot.effective_policy_hash,
        ryeos_state::object_closure::ObjectClosureLimits::default().max_object_bytes,
    )?;
    let policy = ProjectSnapshotPolicy::from_value(&policy_obj)?;
    let target_ignore = state
        .node_policy
        .require::<ryeos_app::node_policy::sections::ingest_ignore::CompiledIngestIgnorePolicy>()?;
    if policy.sync_scope != ProjectSyncScope::AiOnly {
        anyhow::bail!(
            "project.apply-snapshot only accepts ai_only snapshots in v1 (got {:?})",
            policy.sync_scope
        );
    }
    let tree_obj = ryeos_state::object_closure::load_exact_cas_object_with_cas(
        &cas,
        &snapshot.project_tree_hash,
        ryeos_state::object_closure::ObjectClosureLimits::default().max_object_bytes,
    )?;
    let tree = ProjectTree::from_value(&tree_obj)?;
    ryeos_state::project_sync::validate_project_tree_paths(&tree, &policy)?;
    ryeos_state::project_sync::validate_project_tree_against_target_ignore(
        &tree,
        &target_ignore.matcher,
    )?;
    ryeos_state::project_sync::validate_captured_policy_source(&cas, &tree, &policy)?;

    let current_ref = state
        .state_store
        .with_state_db(|db| db.read_deployed_project_ref(&project_hash))?;
    let previous_deployed_hash = current_ref.as_ref().map(|r| r.target_hash.clone());
    let expected_deployed_hash = req.expected_deployed_hash.0.as_deref();
    if previous_deployed_hash.as_deref() != Some(req.snapshot_hash.as_str())
        && previous_deployed_hash.as_deref() != expected_deployed_hash
    {
        anyhow::bail!(
            "deployed project conflict for '{}': expected {:?}, got {:?}",
            canonical_project_path,
            expected_deployed_hash,
            previous_deployed_hash
        );
    }
    // Validate the complete typed snapshot DAG on every apply, including a
    // first deployment. Using the head itself as the search target preserves
    // the traversal while making the ancestry predicate trivially true.
    let ancestry_target = expected_deployed_hash.unwrap_or(&req.snapshot_hash);
    let contains_expected = snapshot_history_contains(&cas, &req.snapshot_hash, ancestry_target)?;
    if let Some(expected) = expected_deployed_hash
        && !contains_expected
    {
        anyhow::bail!(
            "project.apply-snapshot ancestry conflict: target {} does not descend from expected deployed snapshot {}",
            req.snapshot_hash,
            expected
        );
    }
    if previous_deployed_hash.as_deref() == Some(req.snapshot_hash.as_str()) {
        return Ok(serde_json::json!({
            "project_path": canonical_project_path,
            "project_hash": project_hash,
            "snapshot_hash": req.snapshot_hash,
            "previous_deployed_hash": previous_deployed_hash,
            "project_sync_scope": policy.sync_scope,
            "tree_entries": tree.files.len(),
            "idempotent": true,
        }));
    }

    let transaction_id = uuid::Uuid::new_v4().to_string();
    let operation = ApplyOperation {
        operation_type: APPLY_OPERATION_TYPE.to_owned(),
        schema: APPLY_OPERATION_SCHEMA.to_owned(),
        job_id: format!("project-snapshot-apply:{transaction_id}"),
        attempt_id: format!("project-snapshot-apply-attempt:{transaction_id}"),
        transaction_id: transaction_id.clone(),
        project_path: canonical_project_path.clone(),
        project_hash: project_hash.clone(),
        project_identity: project_directory.identity()?,
        previous_deployed_hash: previous_deployed_hash.clone(),
        target_snapshot_hash: req.snapshot_hash.clone(),
        caller_fingerprint: ctx.fingerprint.clone(),
    };
    create_apply_job(&state, &operation)?;
    let staging = match StagingDirectory::create(&project_directory, &transaction_id) {
        Ok(staging) => staging,
        Err(error) => {
            if open_transaction_root(&project_directory, &transaction_id)?.is_some() {
                return Err(
                    error.context("project apply workspace creation requires startup recovery")
                );
            }
            let diagnostic = format!("{error:#}");
            settle_apply_job(
                &state,
                &operation,
                SyncJobState::Failed,
                "workspace_create_failed",
                Some(diagnostic.clone()),
                serde_json::json!({"outcome": "not_started"}),
                true,
            )?;
            return Err(error);
        }
    };
    let staging_root = staging.descriptor_path()?;

    if let Err(error) = materialize_tree_to_staging(&cas, &tree, staging.directory()) {
        drop(cas_read_guard);
        if let Err(cleanup) = staging.cleanup() {
            return Err(error.context(format!(
                "materialization cleanup requires startup recovery: {cleanup:#}"
            )));
        }
        let diagnostic = format!("{error:#}");
        settle_apply_job(
            &state,
            &operation,
            SyncJobState::Failed,
            "materialization_failed",
            Some(diagnostic),
            serde_json::json!({"outcome": "not_started"}),
            true,
        )?;
        return Err(error);
    }
    // The caller-scoped pushed HEAD remains a GC root while we wait for the
    // scheduler-visible commit window. Reacquire the hierarchy in canonical
    // order immediately before deployed-head publication.
    drop(cas_read_guard);

    let mut journal_prepared = false;
    let mut durable_resolution_complete = false;
    let apply_result: Result<ApplyReport> = async {
        let deploy_ctx = ProjectDeployContext {
            project_path: &project_path,
            staging_root: &staging_root,
            tree: &tree,
            snapshot_hash: &req.snapshot_hash,
            project_key: &project_hash,
            caller: &ctx,
            state: &state,
        };

        // Serialize only the scheduler-visible window. plan() must see
        // the same schedule state prepare_commit() mutates, so both sit
        // under the gate, together with root swaps, ref advancement,
        // and their rollbacks. Request validation, CAS reads, and
        // staging materialization scale with project size and run
        // before the gate so timer/recovery dispatch is not blocked
        // behind large applies (per-project serialization is handled
        // by `project_apply_lock` above).
        let _scheduler_guard = state.scheduler_runtime_gate.clone().write_owned().await;
        let deploy_plan = project_deploy::plan(&deploy_ctx)?;
        let recovery_preparation =
            project_deploy::prepare_recovery_before_images(&deploy_plan, &deploy_ctx).await?;
        let schedule_before_images = recovery_preparation.schedule_before_images().to_vec();
        let mut schedule_restore_required = false;
        // CAS mutation guards are deliberately thread-local. Keep the whole
        // surface/ref transaction synchronous under its guard; schedule
        // preparation and restoration may await only outside that lifetime.
        let result = (|| -> Result<ApplyReport> {
            let _cas_publish_guard = authority.acquire_shared_guard()?;
            let _permit = state
                .write_barrier
                .acquire_with_timeout(ryeos_app::write_barrier::ONLINE_WRITE_PERMIT_TIMEOUT)
                .map_err(|e| anyhow!("cannot acquire CAS write permit: {e}"))?;
            project_directory.ensure_path_binding()?;
            let journal_surfaces = capture_journal_surfaces(
                &cas,
                &tree,
                &project_directory,
                staging.directory(),
            )?;
            let journal_key = authority
                .require_recovery()?
                .workspace_journal_auth_key()?;
            write_apply_journal(
                &staging,
                &journal_key,
                ApplyJournal {
                    kind: APPLY_JOURNAL_KIND.to_owned(),
                    schema: APPLY_JOURNAL_SCHEMA,
                    operation: operation.clone(),
                    surfaces: journal_surfaces.clone(),
                    schedule_before_images: schedule_before_images.clone(),
                    auth_tag: String::new(),
                },
            )?;
            journal_prepared = true;
            let mut surface_swap = match replace_managed_surfaces(
                &project_directory,
                &staging,
                &journal_surfaces,
                tree.files.len(),
            ) {
                Ok(surface_swap) => surface_swap,
                Err(error) => {
                    rollback_journal_surfaces(
                        &project_directory,
                        &staging,
                        &journal_surfaces,
                    )?;
                    durable_resolution_complete = true;
                    return Err(error);
                }
            };
            let mut deploy_tx = match project_deploy::prepare_commit_with_recovery(
                &deploy_plan,
                &deploy_ctx,
                recovery_preparation,
            ) {
                Ok(tx) => tx,
                Err(err) => {
                    surface_swap.retain_for_recovery();
                    rollback_journal_surfaces(
                        &project_directory,
                        &staging,
                        &journal_surfaces,
                    )?;
                    schedule_restore_required = true;
                    return Err(err);
                }
            };

            let commit_surface_validation = (|| -> Result<()> {
                project_directory.ensure_path_binding()?;
                verify_live_journal_surfaces(&project_directory, &journal_surfaces, true)?;
                // Recheck after the potentially large content walk. The pinned
                // descriptor remains exact, but the pathname is the subject the
                // deployed ref names and must still resolve to this inode at the
                // publication boundary.
                project_directory.ensure_path_binding()
        })();
        if let Err(error) = commit_surface_validation {
            deploy_tx.rollback(&deploy_ctx);
            surface_swap.retain_for_recovery();
            rollback_journal_surfaces(
                &project_directory,
                &staging,
                &journal_surfaces,
            )
            .context("project apply commit-fence failure also failed surface rollback")?;
            schedule_restore_required = true;
            return Err(error);
        }

        let signer = ryeos_app::state_store::NodeIdentitySigner::from_identity(&state.identity);
        let ref_result = state.state_store.with_state_db(|db| {
            let current = db
                .read_deployed_project_ref(&project_hash)?
                .map(|head| head.target_hash);
            if current.as_deref() == Some(req.snapshot_hash.as_str()) {
                return Ok(());
            }
            if current.as_deref() != expected_deployed_hash {
                anyhow::bail!(
                    "deployed project changed during apply: expected {:?}, got {:?}",
                    expected_deployed_hash,
                    current
                );
            }
            match expected_deployed_hash {
                Some(expected) => db.advance_deployed_project_ref(
                    &project_hash,
                    &req.snapshot_hash,
                    expected,
                    &signer,
                    &_cas_publish_guard,
                ),
                None => db.write_deployed_project_ref(
                    &project_hash,
                    &req.snapshot_hash,
                    &signer,
                    &_cas_publish_guard,
                ),
            }
        });
        if let Err(err) = ref_result {
            let visible = match state
                .state_store
                .with_state_db(|db| db.read_deployed_project_ref(&project_hash))
            {
                Ok(head) => head.map(|head| head.target_hash),
                Err(visibility_error) => {
                    deploy_tx.retain_for_recovery();
                    surface_swap.retain_for_recovery();
                    return Err(err.context(format!(
                        "deployed-ref publication failed and its durable visibility could not be read: {visibility_error:#}; startup recovery is required"
                    )));
                }
            };
            if visible.as_deref() == Some(req.snapshot_hash.as_str()) {
                deploy_tx.retain_for_recovery();
                surface_swap.retain_for_recovery();
                return Err(err.context(
                    "target project ref is visible but publication durability was not proven; startup recovery is required",
                ));
            }
            if visible == previous_deployed_hash {
                deploy_tx.rollback(&deploy_ctx);
                surface_swap.retain_for_recovery();
                rollback_journal_surfaces(
                    &project_directory,
                    &staging,
                    &journal_surfaces,
                )?;
                schedule_restore_required = true;
                return Err(err);
            }
            deploy_tx.retain_for_recovery();
            surface_swap.retain_for_recovery();
            return Err(err.context(format!(
                "deployed project ref has an ambiguous value after publication failure: {visible:?}"
            )));
        }

        if let Err(error) = project_directory
            .ensure_path_binding()
            .and_then(|()| {
                verify_live_journal_surfaces(
                    &project_directory,
                    &journal_surfaces,
                    true,
                )
            })
        {
            // The deployed ref is the durable commit marker. Do not guess at
            // rollback after it succeeded; retain the authenticated journal
            // and active job so startup recovery can verify the target under
            // the exact project identity before settlement.
            deploy_tx.retain_for_recovery();
            surface_swap.retain_for_recovery();
            return Err(error.context(
                "project path or managed surfaces changed after deployed-ref publication; startup recovery is required",
            ));
        }

        let deploy_report = deploy_tx.report.clone();
        deploy_tx.finalize(&deploy_ctx);
        surface_swap.finalize();
        durable_resolution_complete = true;
        let mut report = surface_swap.report.clone();
        report.deploy = deploy_report;
        Ok(report)
        })();
        if schedule_restore_required {
            if let Err(rollback_error) = project_deploy::restore_recovery_before_images(
                &state,
                &schedule_before_images,
            ).await {
                return Err(rollback_error.context(format!(
                    "project apply failed ({:#}); schedule restoration requires startup recovery",
                    result.as_ref().expect_err("schedule restoration only follows a failed apply"),
                )));
            }
            durable_resolution_complete = true;
        }
        result
    }
    .await;
    let retain_for_recovery =
        apply_result.is_err() && journal_prepared && !durable_resolution_complete;
    let cleanup = if retain_for_recovery {
        Ok(())
    } else {
        staging.cleanup()
    };
    cleanup.with_context(|| {
        format!(
            "failed to durably retire project apply transaction {}",
            staging_root.display()
        )
    })?;
    let report = match apply_result {
        Ok(report) => {
            settle_apply_job(
                &state,
                &operation,
                SyncJobState::Completed,
                "completed",
                None,
                serde_json::json!({"outcome": "committed"}),
                true,
            )?;
            report
        }
        Err(error) if retain_for_recovery => return Err(error),
        Err(error) => {
            let diagnostic = format!("{error:#}");
            settle_apply_job(
                &state,
                &operation,
                SyncJobState::Failed,
                "rolled_back",
                Some(diagnostic),
                serde_json::json!({"outcome": "rolled_back"}),
                true,
            )?;
            return Err(error);
        }
    };

    Ok(serde_json::json!({
        "project_path": canonical_project_path,
        "project_hash": project_hash,
        "snapshot_hash": req.snapshot_hash,
        "previous_deployed_hash": previous_deployed_hash,
        "project_sync_scope": policy.sync_scope,
        "tree_entries": tree.files.len(),
        "files_materialized": report.files_materialized,
        "surfaces_replaced": report.surfaces_replaced,
        "surfaces_deleted": report.surfaces_deleted,
        "schedules": {
            "declared": report.deploy.schedules.declared,
            "created": report.deploy.schedules.created,
            "updated": report.deploy.schedules.updated,
            "deleted": report.deploy.schedules.deleted,
        },
    }))
}

/// Per-project logical-ref serialization. This remains separate from the
/// exact Lillux workspace lock because HEAD publication also occurs for
/// projects with no locally opened workspace. A tokio mutex keeps waits off
/// runtime workers; weak registry entries prevent project names from becoming
/// process-lifetime state. Workspace-mutating paths take this lock before the
/// Lillux lock, then the scheduler gate; no path acquires them in reverse.
pub(crate) fn project_apply_lock(project_hash: &str) -> Arc<tokio::sync::Mutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>> = OnceLock::new();
    let locks = LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut locks = locks.lock().unwrap_or_else(|e| e.into_inner());
    locks.retain(|_, lock| lock.strong_count() != 0);
    if let Some(lock) = locks.get(project_hash).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    locks.insert(project_hash.to_owned(), Arc::downgrade(&lock));
    lock
}

/// Shared by project handlers: absolute, canonicalized, existing
/// directory — fail loud otherwise.
pub(crate) fn canonical_existing_project_path(project_path: &str) -> Result<PathBuf> {
    let path = Path::new(project_path);
    if !path.is_absolute() {
        anyhow::bail!("project_path '{}' is not absolute", project_path);
    }
    let canonical = path.canonicalize().with_context(|| {
        format!(
            "cannot canonicalize project_path '{}'; ensure it exists",
            project_path
        )
    })?;
    if !canonical.is_dir() {
        anyhow::bail!("project_path '{}' is not a directory", canonical.display());
    }
    Ok(canonical)
}

pub(crate) fn snapshot_history_contains(cas: &CasStore, head: &str, wanted: &str) -> Result<bool> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum VisitState {
        Visiting,
        Complete,
    }

    let limits = ryeos_state::object_closure::ObjectClosureLimits::for_project_snapshot_transport();
    let mut pending = vec![(head.to_string(), false)];
    let mut states = HashMap::<String, VisitState>::new();
    let mut contains = false;
    while let Some((hash, exiting)) = pending.pop() {
        if exiting {
            states.insert(hash, VisitState::Complete);
            continue;
        }
        match states.get(&hash) {
            Some(VisitState::Visiting) => {
                anyhow::bail!("project snapshot history contains a cycle at {hash}")
            }
            Some(VisitState::Complete) => continue,
            None => {}
        }
        if hash == wanted {
            contains = true;
        }
        states.insert(hash.clone(), VisitState::Visiting);
        if states.len() > limits.max_objects {
            anyhow::bail!("project snapshot ancestry exceeds the object traversal limit");
        }
        let value = ryeos_state::object_closure::load_exact_cas_object_with_cas(
            cas,
            &hash,
            limits.max_object_bytes,
        )?;
        let snapshot = ProjectSnapshot::from_value(&value)?;
        let mut parents = snapshot.parent_hashes;
        if parents.len() > limits.max_links_per_object {
            anyhow::bail!("project snapshot has too many parent links");
        }
        parents.sort();
        pending.push((hash, true));
        for parent in parents.into_iter().rev() {
            pending.push((parent, false));
        }
    }
    Ok(contains)
}

type PinnedDirectory = lillux::PinnedDirectory;

const APPLY_TRANSACTION_DIRECTORY: &str = "project-apply-transactions";
const APPLY_TRANSACTION_ENTRY_LIMIT: usize = 1_000_000;
const APPLY_TRANSACTION_DEPTH_LIMIT: usize = 128;

#[derive(Debug)]
struct StagingDirectory {
    parent: PinnedDirectory,
    transaction: PinnedDirectory,
    directory: PinnedDirectory,
    backup: PinnedDirectory,
    discard: PinnedDirectory,
    name: OsString,
}

impl StagingDirectory {
    fn create(project: &PinnedDirectory, transaction_id: &str) -> Result<Self> {
        let project_ai = project.open_or_create_child(OsStr::new(ryeos_engine::AI_DIR), 0o755)?;
        let project_state = project_ai.open_or_create_child(OsStr::new("state"), 0o700)?;
        let parent =
            project_state.open_or_create_child(OsStr::new(APPLY_TRANSACTION_DIRECTORY), 0o700)?;
        let name = OsString::from(transaction_id);
        let transaction = parent
            .create_child(&name, 0o700)
            .context("create project apply transaction")?;
        let directory = transaction.create_child(OsStr::new("staging"), 0o700)?;
        let backup = transaction.create_child(OsStr::new("backup"), 0o700)?;
        let discard = transaction.create_child(OsStr::new("discard"), 0o700)?;
        Ok(Self {
            parent: parent.try_clone()?,
            transaction,
            directory,
            backup,
            discard,
            name,
        })
    }

    fn open(project: &PinnedDirectory, transaction_id: &str) -> Result<Option<Self>> {
        let Some((parent, transaction, name)) = open_transaction_root(project, transaction_id)?
        else {
            return Ok(None);
        };
        let directory = transaction
            .open_child_directory(OsStr::new("staging"))?
            .context("project apply transaction has no staging directory")?;
        let backup = transaction
            .open_child_directory(OsStr::new("backup"))?
            .context("project apply transaction has no backup directory")?;
        let discard = transaction
            .open_child_directory(OsStr::new("discard"))?
            .context("project apply transaction has no discard directory")?;
        Ok(Some(Self {
            parent,
            transaction,
            directory,
            backup,
            discard,
            name,
        }))
    }

    fn directory(&self) -> &PinnedDirectory {
        &self.directory
    }

    fn descriptor_path(&self) -> Result<PathBuf> {
        self.directory.descriptor_path()
    }

    fn cleanup(&self) -> Result<()> {
        cleanup_transaction_root(&self.parent, &self.transaction, &self.name)
    }
}

fn open_transaction_root(
    project: &PinnedDirectory,
    transaction_id: &str,
) -> Result<Option<(PinnedDirectory, PinnedDirectory, OsString)>> {
    let Some(project_ai) = project.open_child_directory(OsStr::new(ryeos_engine::AI_DIR))? else {
        return Ok(None);
    };
    let Some(project_state) = project_ai.open_child_directory(OsStr::new("state"))? else {
        return Ok(None);
    };
    let Some(parent) =
        project_state.open_child_directory(OsStr::new(APPLY_TRANSACTION_DIRECTORY))?
    else {
        return Ok(None);
    };
    let name = OsString::from(transaction_id);
    let Some(transaction) = parent.open_child_directory(&name)? else {
        return Ok(None);
    };
    Ok(Some((parent, transaction, name)))
}

fn cleanup_transaction_root(
    parent: &PinnedDirectory,
    transaction: &PinnedDirectory,
    name: &OsStr,
) -> Result<()> {
    transaction.remove_contents_recursive_bounded(lillux::DirectoryTraversalBudget::new(
        APPLY_TRANSACTION_ENTRY_LIMIT,
        APPLY_TRANSACTION_DEPTH_LIMIT,
    ))?;
    if !parent.remove_empty_child_if_same(name, transaction)? {
        anyhow::bail!("project apply transaction remained non-empty during cleanup");
    }
    Ok(())
}

fn capture_journal_surfaces(
    cas: &CasStore,
    tree: &ProjectTree,
    project: &PinnedDirectory,
    staging: &PinnedDirectory,
) -> Result<Vec<JournalSurface>> {
    let expected_targets = expected_target_surface_identities(cas, tree)?;
    let surfaces = observe_journal_surfaces(project, staging)?;
    verify_journal_target_identities(&surfaces, &expected_targets)?;
    Ok(surfaces)
}

fn observe_journal_surfaces(
    project: &PinnedDirectory,
    staging: &PinnedDirectory,
) -> Result<Vec<JournalSurface>> {
    ryeos_state::project_sync::materialized_project_ai_surfaces()
        .map(|surface| {
            let prior_content = observe_surface_content(project, surface)?;
            let target_content = observe_surface_content(staging, surface)?;
            Ok(JournalSurface {
                relative: surface.root.to_owned(),
                prior_content,
                target_content,
            })
        })
        .collect()
}

fn expected_target_surface_identities_for_snapshot(
    cas: &CasStore,
    snapshot_hash: &str,
) -> Result<HashMap<String, Option<SurfaceContentIdentity>>> {
    let limits = ryeos_state::object_closure::ObjectClosureLimits::for_project_snapshot_transport();
    let snapshot_value = ryeos_state::object_closure::load_exact_cas_object_with_cas(
        cas,
        snapshot_hash,
        limits.max_object_bytes,
    )?;
    let snapshot = ProjectSnapshot::from_value(&snapshot_value)?;
    let tree_value = ryeos_state::object_closure::load_exact_cas_object_with_cas(
        cas,
        &snapshot.project_tree_hash,
        limits.max_object_bytes,
    )?;
    let tree = ProjectTree::from_value(&tree_value)?;
    expected_target_surface_identities(cas, &tree)
}

fn validate_surface_content_identity(identity: &SurfaceContentIdentity) -> Result<()> {
    let limits = ryeos_state::object_closure::ObjectClosureLimits::for_project_snapshot_transport();
    if identity.schema != SURFACE_CONTENT_SCHEMA
        || !lillux::valid_hash(&identity.digest)
        || identity
            .digest
            .bytes()
            .any(|byte| byte.is_ascii_uppercase())
        || identity.regular_files
            > u64::try_from(ryeos_state::project_sync::MAX_PROJECT_TREE_FILES)
                .expect("project file limit fits u64")
        || identity.directories
            > u64::try_from(ryeos_state::project_sync::MAX_PROJECT_TREE_ENTRIES)
                .expect("project entry limit fits u64")
        || identity
            .regular_files
            .checked_add(identity.directories)
            .is_none_or(|entries| {
                entries
                    > u64::try_from(ryeos_state::project_sync::MAX_PROJECT_TREE_ENTRIES)
                        .expect("project entry limit fits u64")
            })
        || identity.total_bytes > limits.max_total_blob_bytes
    {
        anyhow::bail!("project managed-surface content identity is invalid");
    }
    Ok(())
}

fn surface_content_identity(
    surface: ryeos_state::project_sync::ProjectAiSurface,
    directories: &BTreeSet<String>,
    files: &[SurfaceContentFile],
) -> Result<SurfaceContentIdentity> {
    let total_bytes = files.iter().try_fold(0_u64, |total, file| {
        total
            .checked_add(file.size)
            .ok_or_else(|| anyhow!("managed-surface byte count overflow"))
    })?;
    let shape = match surface.shape {
        ryeos_state::project_sync::ProjectAiSurfaceShape::File => "file",
        ryeos_state::project_sync::ProjectAiSurfaceShape::Directory => "directory",
    };
    let body = serde_json::json!({
        "schema": SURFACE_CONTENT_SCHEMA,
        "surface": surface.root,
        "shape": shape,
        "directories": directories,
        "files": files,
    });
    let identity = SurfaceContentIdentity {
        schema: SURFACE_CONTENT_SCHEMA,
        digest: lillux::sha256_hex(lillux::canonical_json(&body)?.as_bytes()),
        regular_files: u64::try_from(files.len()).context("surface file count exceeds u64")?,
        directories: u64::try_from(directories.len())
            .context("surface directory count exceeds u64")?,
        total_bytes,
    };
    validate_surface_content_identity(&identity)?;
    Ok(identity)
}

fn expected_target_surface_identities(
    cas: &CasStore,
    tree: &ProjectTree,
) -> Result<HashMap<String, Option<SurfaceContentIdentity>>> {
    let limits = ryeos_state::object_closure::ObjectClosureLimits::for_project_snapshot_transport();
    let mut identities = HashMap::new();
    for surface in ryeos_state::project_sync::materialized_project_ai_surfaces() {
        let mut directories = BTreeSet::new();
        let mut files = Vec::new();
        for (project_relative, file_hash) in &tree.files {
            let surface_relative = match surface.shape {
                ryeos_state::project_sync::ProjectAiSurfaceShape::File => {
                    (project_relative == surface.root).then_some("")
                }
                ryeos_state::project_sync::ProjectAiSurfaceShape::Directory => project_relative
                    .strip_prefix(surface.root)
                    .and_then(|suffix| suffix.strip_prefix('/')),
            };
            let Some(surface_relative) = surface_relative else {
                continue;
            };
            let value = ryeos_state::object_closure::load_exact_cas_object_with_cas(
                cas,
                file_hash,
                limits.max_object_bytes,
            )?;
            let file = ProjectFile::from_value(&value)?;
            let mut ancestor = Path::new(surface_relative).parent();
            while let Some(directory_path) = ancestor {
                if directory_path.as_os_str().is_empty() {
                    break;
                }
                let directory = directory_path
                    .to_str()
                    .context("project surface directory path is not UTF-8")?
                    .to_owned();
                ryeos_state::project_sync::validate_safe_relative_path(&directory)?;
                directories.insert(directory);
                ancestor = directory_path.parent();
            }
            files.push(SurfaceContentFile {
                path: surface_relative.to_owned(),
                blob_hash: file.blob_hash,
                size: file.size,
                normalized_mode: file.normalized_mode,
            });
        }
        files.sort_by(|left, right| left.path.cmp(&right.path));
        let present = !files.is_empty();
        let identity = present
            .then(|| surface_content_identity(surface, &directories, &files))
            .transpose()?;
        identities.insert(surface.root.to_owned(), identity);
    }
    Ok(identities)
}

fn observe_surface_content(
    root: &PinnedDirectory,
    surface: ryeos_state::project_sync::ProjectAiSurface,
) -> Result<Option<SurfaceContentIdentity>> {
    let Some((parent, name)) =
        crate::project_namespace::relative_parent(root, surface.root, false)?
    else {
        return Ok(None);
    };
    let Some(entry) = parent.entry_no_follow(&name)? else {
        return Ok(None);
    };
    let limits = ryeos_state::object_closure::ObjectClosureLimits::for_project_snapshot_transport();
    let mut directories = BTreeSet::new();
    let mut files = Vec::new();
    match surface.shape {
        ryeos_state::project_sync::ProjectAiSurfaceShape::File => {
            if entry.entry_type != lillux::PinnedEntryType::Regular {
                anyhow::bail!("managed surface '{}' is not a regular file", surface.root);
            }
            let file = parent
                .open_pinned_regular(&name, false)?
                .context("managed surface regular file disappeared")?;
            let observation = file.observation()?;
            if !observation.matches_directory_entry(&entry) {
                anyhow::bail!("managed surface changed during content observation");
            }
            if observation.size() > limits.max_blob_bytes {
                anyhow::bail!("managed surface '{}' exceeds its byte bound", surface.root);
            }
            files.push(SurfaceContentFile {
                path: String::new(),
                blob_hash: file.digest_stable_exact(&observation)?,
                size: observation.size(),
                normalized_mode: observation.portable_mode()?,
            });
            parent.ensure_entry_observation(&entry)?;
        }
        ryeos_state::project_sync::ProjectAiSurfaceShape::Directory => {
            if entry.entry_type != lillux::PinnedEntryType::Directory {
                anyhow::bail!("managed surface '{}' is not a directory", surface.root);
            }
            let directory = parent
                .open_child_directory(&name)?
                .context("managed surface directory disappeared")?;
            let (containing_device, inode) = directory.device_inode()?;
            if containing_device != entry.containing_device || inode != entry.inode {
                anyhow::bail!("managed surface directory changed during content observation");
            }
            let mut total_bytes = 0_u64;
            directory.visit_regular_files_bounded(
                lillux::DirectoryTraversalBudget::new(
                    ryeos_state::project_sync::MAX_PROJECT_TREE_ENTRIES,
                    ryeos_state::project_sync::MAX_PROJECT_TREE_DEPTH,
                ),
                |relative, is_directory| {
                    let relative = relative
                        .to_str()
                        .context("managed surface contains a non-UTF-8 path")?;
                    ryeos_state::project_sync::validate_safe_relative_path(relative)?;
                    if is_directory {
                        directories.insert(relative.to_owned());
                    }
                    Ok(false)
                },
                |relative, file| {
                    if files.len() >= ryeos_state::project_sync::MAX_PROJECT_TREE_FILES {
                        anyhow::bail!("managed surface exceeds its regular-file bound");
                    }
                    let relative = relative
                        .to_str()
                        .context("managed surface contains a non-UTF-8 file path")?
                        .to_owned();
                    let observation = lillux::observe_open_regular_file(&file)?;
                    if observation.size() > limits.max_blob_bytes {
                        anyhow::bail!("managed surface file exceeds its byte bound");
                    }
                    total_bytes = total_bytes
                        .checked_add(observation.size())
                        .context("managed surface byte count overflow")?;
                    if total_bytes > limits.max_total_blob_bytes {
                        anyhow::bail!("managed surface exceeds its total-byte bound");
                    }
                    let mut file = file;
                    let (blob_hash, metadata) = lillux::digest_open_regular_file_stable_exact(
                        &mut file,
                        observation.size(),
                    )?;
                    files.push(SurfaceContentFile {
                        path: relative,
                        blob_hash,
                        size: observation.size(),
                        normalized_mode: lillux::normalized_portable_regular_mode(&metadata)?,
                    });
                    Ok(())
                },
            )?;
            parent.ensure_entry_observation(&entry)?;
        }
    }
    files.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(Some(surface_content_identity(
        surface,
        &directories,
        &files,
    )?))
}

fn authenticate_apply_journal(key: &[u8; 32], journal: &ApplyJournal) -> Result<String> {
    let authenticated = lillux::canonical_json(&journal.authenticated_value()?)?;
    let mut mac = <Hmac<Sha256>>::new_from_slice(key)
        .map_err(|_| anyhow!("invalid workspace journal authentication key"))?;
    mac.update(authenticated.as_bytes());
    Ok(mac
        .finalize()
        .into_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn write_apply_journal(
    workspace: &StagingDirectory,
    key: &[u8; 32],
    mut journal: ApplyJournal,
) -> Result<()> {
    journal.auth_tag = authenticate_apply_journal(key, &journal)?;
    let bytes = lillux::canonical_json(&serde_json::to_value(&journal)?)?.into_bytes();
    if bytes.len() as u64 > APPLY_JOURNAL_MAX_BYTES {
        anyhow::bail!("project apply journal exceeds its byte limit");
    }
    if workspace
        .transaction
        .atomic_create_regular(OsStr::new("journal.json"), &bytes, 0o600)?
        .is_none()
    {
        anyhow::bail!("project apply journal already exists");
    }
    workspace.transaction.sync()?;
    Ok(())
}

#[cfg(test)]
fn read_apply_journal(workspace: &StagingDirectory, key: &[u8; 32]) -> Result<ApplyJournal> {
    read_apply_journal_from_transaction(&workspace.transaction, key)
}

fn read_apply_journal_from_transaction(
    transaction: &PinnedDirectory,
    key: &[u8; 32],
) -> Result<ApplyJournal> {
    let file = transaction
        .open_pinned_regular(OsStr::new("journal.json"), false)?
        .context("project apply transaction has no authenticated journal")?;
    let bytes = file.read_bounded(APPLY_JOURNAL_MAX_BYTES)?;
    let journal: ApplyJournal = serde_json::from_slice(&bytes)?;
    if lillux::canonical_json(&serde_json::to_value(&journal)?)?.as_bytes() != bytes {
        anyhow::bail!("project apply journal is not canonical JSON");
    }
    if journal.kind != APPLY_JOURNAL_KIND || journal.schema != APPLY_JOURNAL_SCHEMA {
        anyhow::bail!("unsupported project apply journal contract");
    }
    let supplied = decode_auth_tag(&journal.auth_tag)?;
    let authenticated = lillux::canonical_json(&journal.authenticated_value()?)?;
    let mut mac = <Hmac<Sha256>>::new_from_slice(key)
        .map_err(|_| anyhow!("invalid workspace journal authentication key"))?;
    mac.update(authenticated.as_bytes());
    mac.verify_slice(&supplied)
        .map_err(|_| anyhow!("project apply journal is not authenticated by this node"))?;
    validate_apply_operation(&journal.operation)?;
    validate_journal_surfaces(&journal.surfaces)?;
    Ok(journal)
}

fn decode_auth_tag(value: &str) -> Result<[u8; 32]> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        anyhow::bail!("project apply journal auth_tag is not a SHA-256 hex digest");
    }
    let mut bytes = [0_u8; 32];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        let pair = std::str::from_utf8(pair)?;
        bytes[index] = u8::from_str_radix(pair, 16)?;
    }
    Ok(bytes)
}

fn validate_apply_operation(operation: &ApplyOperation) -> Result<()> {
    if operation.operation_type != APPLY_OPERATION_TYPE
        || operation.schema != APPLY_OPERATION_SCHEMA
    {
        anyhow::bail!("unsupported project apply operation contract");
    }
    if operation.job_id.is_empty()
        || operation.attempt_id.is_empty()
        || operation.transaction_id.is_empty()
    {
        anyhow::bail!("project apply operation has an empty durable coordinate");
    }
    let transaction = uuid::Uuid::parse_str(&operation.transaction_id)
        .context("project apply transaction_id is not a UUID")?;
    if transaction.to_string() != operation.transaction_id
        || operation.job_id != format!("project-snapshot-apply:{}", operation.transaction_id)
        || operation.attempt_id
            != format!(
                "project-snapshot-apply-attempt:{}",
                operation.transaction_id
            )
    {
        anyhow::bail!("project apply operation has non-canonical durable coordinates");
    }
    let path = Path::new(&operation.project_path);
    let canonical_components = path.components().collect::<PathBuf>();
    if path.components().count() < 2
        || !path.is_absolute()
        || path.components().any(|component| {
            !matches!(
                component,
                std::path::Component::RootDir | std::path::Component::Normal(_)
            )
        })
        || canonical_components.to_str() != Some(operation.project_path.as_str())
        || operation.project_hash
            != ryeos_state::refs::deployed_project_key(&operation.project_path)
    {
        anyhow::bail!("project apply operation has invalid project identity");
    }
    validate_canonical_object_hash(
        "project apply target_snapshot_hash",
        &operation.target_snapshot_hash,
    )?;
    if let Some(previous) = &operation.previous_deployed_hash {
        validate_canonical_object_hash("project apply previous_deployed_hash", previous)?;
    }
    ryeos_state::refs::principal_storage_key(&operation.caller_fingerprint)
        .context("project apply caller fingerprint is invalid")?;
    Ok(())
}

fn validate_canonical_object_hash(label: &str, value: &str) -> Result<()> {
    if !lillux::valid_hash(value) || value.bytes().any(|byte| byte.is_ascii_uppercase()) {
        anyhow::bail!("{label} is not a canonical lowercase SHA-256 digest");
    }
    Ok(())
}

fn create_apply_job(state: &AppState, operation: &ApplyOperation) -> Result<SyncJobRecord> {
    validate_apply_operation(operation)?;
    let mut roots = vec![operation.target_snapshot_hash.clone()];
    if let Some(previous) = &operation.previous_deployed_hash {
        roots.push(previous.clone());
    }
    let job = state.state_store.with_state_db(|db| {
        db.create_sync_job(&NewSyncJob {
            job_id: operation.job_id.clone(),
            operation_type: APPLY_OPERATION_TYPE.to_owned(),
            operation: serde_json::to_value(operation)?,
            peer: None,
            roots,
            heads: Vec::new(),
            max_attempts: 1,
        })
    })?;
    if let Err(error) = state.state_store.with_state_db(|db| {
        db.create_sync_job_attempt(&NewSyncJobAttempt {
            attempt_id: operation.attempt_id.clone(),
            job_id: operation.job_id.clone(),
            worker_id: Some("project.apply-snapshot".to_owned()),
            phase: "preparing_workspace".to_owned(),
        })
    }) {
        let diagnostic = format!("{error:#}");
        settle_apply_job(
            state,
            operation,
            SyncJobState::Failed,
            "attempt_reservation_failed",
            Some(diagnostic),
            serde_json::json!({"outcome": "not_started"}),
            false,
        )?;
        return Err(error);
    }
    Ok(job)
}

fn settle_apply_job(
    state: &AppState,
    operation: &ApplyOperation,
    job_state: SyncJobState,
    phase: &str,
    error: Option<String>,
    result: Value,
    finish_running_attempt: bool,
) -> Result<()> {
    let job = state
        .state_store
        .with_state_db(|db| db.get_sync_job(&operation.job_id))?
        .context("project apply sync job disappeared")?;
    let update = SyncJobUpdate {
        state: job_state,
        phase: phase.to_owned(),
        roots: None,
        heads: Some(
            (job_state == SyncJobState::Completed)
                .then(|| vec![operation.target_snapshot_hash.clone()])
                .unwrap_or_default(),
        ),
        uploaded_hashes: job.uploaded_hashes,
        fetched_hashes: job.fetched_hashes,
        last_error: error.clone(),
        result: Some(result.clone()),
    };
    state.state_store.with_state_db(|db| {
        if finish_running_attempt {
            db.finish_sync_job_attempt_and_update_job(
                &operation.attempt_id,
                &FinishSyncJobAttempt {
                    state: if job_state == SyncJobState::Completed {
                        SyncJobAttemptState::Completed
                    } else {
                        SyncJobAttemptState::Failed
                    },
                    phase: phase.to_owned(),
                    error,
                    result: Some(result),
                },
                &operation.job_id,
                &update,
            )
        } else {
            db.update_sync_job(&operation.job_id, &update)
        }
    })
}

fn materialize_tree_to_staging(
    cas: &CasStore,
    tree: &ProjectTree,
    staging_root: &PinnedDirectory,
) -> Result<usize> {
    let mut count = 0usize;
    for (rel_path, file_hash) in &tree.files {
        // Floors (secrets/node-owned) are enforced regardless; ignore was
        // already validated against the live matcher by the caller.
        ryeos_state::project_sync::validate_project_manifest_path(
            rel_path,
            ProjectSyncScope::AiOnly,
            None,
        )?;
        let limits =
            ryeos_state::object_closure::ObjectClosureLimits::for_project_snapshot_transport();
        let file_obj = ryeos_state::object_closure::load_exact_cas_object_with_cas(
            cas,
            file_hash,
            limits.max_object_bytes,
        )?;
        let file = ProjectFile::from_value(&file_obj)?;
        write_staged_regular_file(
            staging_root,
            Path::new(rel_path),
            cas,
            &file.blob_hash,
            file.size,
            Some(file.normalized_mode),
        )?;
        count += 1;
    }
    Ok(count)
}

fn write_staged_regular_file(
    staging_root: &PinnedDirectory,
    relative: &Path,
    cas: &CasStore,
    blob_hash: &str,
    expected_size: u64,
    mode: Option<u32>,
) -> Result<()> {
    let relative = relative
        .to_str()
        .ok_or_else(|| anyhow!("project path is not valid UTF-8"))?;
    let (parent, name) =
        crate::project_namespace::relative_parent_with_mode(staging_root, relative, true, 0o755)?
            .ok_or_else(|| anyhow!("staging path has no filename"))?;
    let copied = cas.materialize_blob_to_new_regular(
        blob_hash,
        &parent,
        &name,
        mode.unwrap_or(ProjectFile::REGULAR_MODE),
    )?;
    if copied != expected_size {
        anyhow::bail!("project blob {blob_hash} has {copied} bytes, expected {expected_size}");
    }
    Ok(())
}

#[cfg(test)]
fn apply_mode(path: &Path, mode: Option<u32>) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = mode.unwrap_or(0o644) & 0o777;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
    Ok(())
}

#[derive(Debug, Clone, Default)]
struct ApplyReport {
    files_materialized: usize,
    surfaces_replaced: usize,
    surfaces_deleted: usize,
    deploy: project_deploy::ProjectDeployReport,
}

#[derive(Debug)]
struct SurfaceSwap {
    relative: String,
    prior_present: bool,
    installed: bool,
}

#[derive(Debug)]
struct PreparedSurfaceSwap {
    project: PinnedDirectory,
    staging: PinnedDirectory,
    backup: PinnedDirectory,
    discard: PinnedDirectory,
    swaps: Vec<SurfaceSwap>,
    report: ApplyReport,
    finalized: bool,
}

impl PreparedSurfaceSwap {
    fn rollback(&mut self) -> Result<()> {
        let result = rollback_swaps(
            &self.project,
            &self.staging,
            &self.backup,
            &self.discard,
            &self.swaps,
        );
        self.finalized = true;
        result
    }

    fn finalize(&mut self) {
        self.finalized = true;
    }

    fn retain_for_recovery(&mut self) {
        self.finalized = true;
    }
}

impl Drop for PreparedSurfaceSwap {
    fn drop(&mut self) {
        debug_assert!(self.finalized, "project surface swap dropped unresolved");
    }
}

fn replace_managed_surfaces(
    project_root: &PinnedDirectory,
    workspace: &StagingDirectory,
    journal_surfaces: &[JournalSurface],
    files_materialized: usize,
) -> Result<PreparedSurfaceSwap> {
    let mut prepared = PreparedSurfaceSwap {
        project: project_root.try_clone()?,
        staging: workspace.directory.try_clone()?,
        backup: workspace.backup.try_clone()?,
        discard: workspace.discard.try_clone()?,
        swaps: Vec::new(),
        report: ApplyReport {
            files_materialized,
            ..ApplyReport::default()
        },
        finalized: false,
    };
    let result = (|| -> Result<()> {
        let declared =
            ryeos_state::project_sync::materialized_project_ai_surfaces().collect::<Vec<_>>();
        if declared.len() != journal_surfaces.len() {
            anyhow::bail!("project apply journal has the wrong managed-surface count");
        }
        for (surface, frozen) in declared.into_iter().zip(journal_surfaces) {
            if frozen.relative != surface.root {
                anyhow::bail!("project apply journal managed-surface order changed");
            }
            let live =
                crate::project_namespace::relative_parent(&prepared.project, surface.root, false)?;
            let staged =
                crate::project_namespace::relative_parent(&prepared.staging, surface.root, false)?;
            let prior_content = observe_surface_content(&prepared.project, surface)?;
            let target_content = observe_surface_content(&prepared.staging, surface)?;
            if prior_content != frozen.prior_content || target_content != frozen.target_content {
                anyhow::bail!("project managed surface changed after durable preparation");
            }
            let prior_present = prior_content.is_some();
            let target_present = target_content.is_some();
            prepared.swaps.push(SurfaceSwap {
                relative: surface.root.to_owned(),
                prior_present,
                installed: false,
            });
            let swap = prepared
                .swaps
                .last_mut()
                .expect("surface swap was just appended");
            if prior_present {
                let (live_parent, name) = live.as_ref().expect("present live surface has a parent");
                let (backup_parent, _) = crate::project_namespace::relative_parent(
                    &prepared.backup,
                    surface.root,
                    true,
                )?
                .ok_or_else(|| anyhow!("backup surface path has no filename"))?;
                move_exact_child(live_parent, &backup_parent, name)?;
            }
            if target_present {
                let (staged_parent, name) = staged
                    .as_ref()
                    .expect("present staged surface has a parent");
                let (live_parent, _) = crate::project_namespace::relative_parent_with_mode(
                    &prepared.project,
                    surface.root,
                    true,
                    0o755,
                )?
                .ok_or_else(|| anyhow!("live surface path has no filename"))?;
                move_exact_child(staged_parent, &live_parent, name)?;
                swap.installed = true;
                prepared.report.surfaces_replaced += 1;
            } else {
                prepared.report.surfaces_deleted += usize::from(prior_present);
            }
        }
        Ok(())
    })();
    if let Err(error) = result {
        let rollback = prepared.rollback();
        return match rollback {
            Ok(()) => Err(error),
            Err(rollback) => {
                prepared.retain_for_recovery();
                Err(error.context(format!(
                    "project surface rollback also failed: {rollback:#}"
                )))
            }
        };
    }
    Ok(prepared)
}

fn rollback_swaps(
    project: &PinnedDirectory,
    _staging: &PinnedDirectory,
    backup: &PinnedDirectory,
    discard: &PinnedDirectory,
    swaps: &[SurfaceSwap],
) -> Result<()> {
    for swap in swaps.iter().rev() {
        let live = crate::project_namespace::relative_parent(project, &swap.relative, false)?;
        if swap.installed
            && let Some((live_parent, name)) = live.as_ref()
            && live_parent.entry_no_follow(name)?.is_some()
        {
            let (discard_parent, _) =
                crate::project_namespace::relative_parent(discard, &swap.relative, true)?
                    .ok_or_else(|| anyhow!("discard surface path has no filename"))?;
            move_exact_child(live_parent, &discard_parent, name)?;
        }
        if swap.prior_present {
            let backup_entry =
                crate::project_namespace::relative_parent(backup, &swap.relative, false)?;
            if let Some((backup_parent, name)) = backup_entry
                && backup_parent.entry_no_follow(&name)?.is_some()
            {
                let (live_parent, _) = crate::project_namespace::relative_parent_with_mode(
                    project,
                    &swap.relative,
                    true,
                    0o755,
                )?
                .ok_or_else(|| anyhow!("live surface path has no filename"))?;
                move_exact_child(&backup_parent, &live_parent, &name)?;
            }
        }
    }
    Ok(())
}

fn move_exact_child(
    source: &PinnedDirectory,
    destination: &PinnedDirectory,
    name: &OsStr,
) -> Result<()> {
    let entry = source
        .entry_no_follow(name)?
        .ok_or_else(|| anyhow!("project surface disappeared before move"))?;
    if !source
        .move_child_if_same_noreplace_to(&entry, destination)
        .map_err(anyhow::Error::from)?
    {
        anyhow::bail!(
            "project surface destination already exists: {}",
            destination.path().join(name).display()
        );
    }
    Ok(())
}

fn validate_journal_surfaces(surfaces: &[JournalSurface]) -> Result<()> {
    let declared =
        ryeos_state::project_sync::materialized_project_ai_surfaces().collect::<Vec<_>>();
    if declared.len() != surfaces.len() {
        anyhow::bail!("project apply journal has the wrong managed-surface count");
    }
    for (surface, frozen) in declared.into_iter().zip(surfaces) {
        if surface.root != frozen.relative {
            anyhow::bail!("project apply journal managed-surface order changed");
        }
        if let Some(identity) = &frozen.prior_content {
            validate_surface_content_identity(identity)?;
        }
        if let Some(identity) = &frozen.target_content {
            validate_surface_content_identity(identity)?;
        }
    }
    Ok(())
}

fn verify_journal_target_identities(
    surfaces: &[JournalSurface],
    expected_targets: &HashMap<String, Option<SurfaceContentIdentity>>,
) -> Result<()> {
    validate_journal_surfaces(surfaces)?;
    if expected_targets.len()
        != ryeos_state::project_sync::materialized_project_ai_surfaces().count()
    {
        anyhow::bail!("project snapshot has the wrong managed-surface identity count");
    }
    for frozen in surfaces {
        let expected = expected_targets
            .get(&frozen.relative)
            .context("target surface identity was not derived from the project snapshot")?;
        if &frozen.target_content != expected {
            anyhow::bail!(
                "authenticated target for managed surface '{}' differs from the exact project snapshot",
                frozen.relative
            );
        }
    }
    Ok(())
}

fn live_journal_surfaces_match(
    project: &PinnedDirectory,
    surfaces: &[JournalSurface],
    target: bool,
) -> Result<bool> {
    validate_journal_surfaces(surfaces)?;
    for (surface, frozen) in
        ryeos_state::project_sync::materialized_project_ai_surfaces().zip(surfaces)
    {
        let expected = if target {
            &frozen.target_content
        } else {
            &frozen.prior_content
        };
        if &observe_surface_content(project, surface)? != expected {
            return Ok(false);
        }
    }
    Ok(true)
}

fn verify_live_journal_surfaces(
    project: &PinnedDirectory,
    surfaces: &[JournalSurface],
    target: bool,
) -> Result<()> {
    if !live_journal_surfaces_match(project, surfaces, target)? {
        anyhow::bail!(
            "live managed surfaces do not match the authenticated {} content",
            if target { "target" } else { "prior" }
        );
    }
    Ok(())
}

fn verify_live_target_identities(
    project: &PinnedDirectory,
    expected_targets: &HashMap<String, Option<SurfaceContentIdentity>>,
) -> Result<()> {
    if expected_targets.len()
        != ryeos_state::project_sync::materialized_project_ai_surfaces().count()
    {
        anyhow::bail!("project snapshot has the wrong managed-surface identity count");
    }
    for surface in ryeos_state::project_sync::materialized_project_ai_surfaces() {
        let expected = expected_targets
            .get(surface.root)
            .context("target surface identity was not derived from the project snapshot")?;
        if &observe_surface_content(project, surface)? != expected {
            anyhow::bail!(
                "live managed surface '{}' differs from the exact deployed project snapshot",
                surface.root
            );
        }
    }
    Ok(())
}

fn rollback_journal_surfaces(
    project: &PinnedDirectory,
    workspace: &StagingDirectory,
    surfaces: &[JournalSurface],
) -> Result<()> {
    let declared =
        ryeos_state::project_sync::materialized_project_ai_surfaces().collect::<Vec<_>>();
    validate_journal_surfaces(surfaces)?;
    for (surface, frozen) in declared.into_iter().zip(surfaces).rev() {
        let live = crate::project_namespace::relative_parent(project, surface.root, false)?;
        let backup =
            crate::project_namespace::relative_parent(&workspace.backup, surface.root, false)?;
        let live_content = observe_surface_content(project, surface)?;
        let backup_content = observe_surface_content(&workspace.backup, surface)?;
        if let Some(backup_content) = backup_content {
            if frozen.prior_content.as_ref() != Some(&backup_content) {
                anyhow::bail!("project apply backup contradicts its authenticated journal");
            }
            if let Some(live_content) = &live_content {
                if frozen.target_content.as_ref() != Some(live_content) {
                    anyhow::bail!(
                        "live project surface '{}' is neither its authenticated target nor an absent swap position",
                        surface.root
                    );
                }
                let (live_parent, name) = live.as_ref().expect("present live surface has a parent");
                let (discard_parent, _) = crate::project_namespace::relative_parent(
                    &workspace.discard,
                    surface.root,
                    true,
                )?
                .context("discard surface path has no filename")?;
                move_exact_child(live_parent, &discard_parent, name)?;
            }
            let (backup_parent, name) = backup
                .as_ref()
                .expect("present backup surface has a parent");
            let (live_parent, _) = crate::project_namespace::relative_parent_with_mode(
                project,
                surface.root,
                true,
                0o755,
            )?
            .context("live surface path has no filename")?;
            move_exact_child(backup_parent, &live_parent, name)?;
        } else if live_content == frozen.prior_content {
            // The surface was never moved or a prior recovery pass already
            // restored it. Exact content, rather than presence, proves this.
        } else if frozen.prior_content.is_some() {
            anyhow::bail!(
                "project apply lost or changed the authenticated prior content of '{}'",
                surface.root
            );
        } else if let Some(live_content) = &live_content {
            if frozen.target_content.as_ref() != Some(live_content) {
                anyhow::bail!(
                    "live project surface '{}' has content outside the authenticated transaction",
                    surface.root
                );
            }
            let (live_parent, name) = live.as_ref().expect("present live surface has a parent");
            let (discard_parent, _) =
                crate::project_namespace::relative_parent(&workspace.discard, surface.root, true)?
                    .context("discard surface path has no filename")?;
            move_exact_child(live_parent, &discard_parent, name)?;
        }
    }
    verify_live_journal_surfaces(project, surfaces, false)
}

fn validate_job_operation(job: &SyncJobRecord) -> Result<ApplyOperation> {
    if job.operation_type != APPLY_OPERATION_TYPE {
        anyhow::bail!("project apply sync job has the wrong operation type");
    }
    let operation: ApplyOperation = serde_json::from_value(job.operation.clone())?;
    validate_apply_operation(&operation)?;
    if operation.job_id != job.job_id {
        anyhow::bail!("project apply sync job identity does not match its operation");
    }
    if job
        .roots
        .iter()
        .all(|hash| hash != &operation.target_snapshot_hash)
    {
        anyhow::bail!("project apply sync job does not root its target snapshot");
    }
    if operation
        .previous_deployed_hash
        .as_ref()
        .is_some_and(|previous| job.roots.iter().all(|hash| hash != previous))
    {
        anyhow::bail!("project apply sync job does not root its predecessor snapshot");
    }
    Ok(operation)
}

fn ensure_no_active_apply_job_for_project(state: &AppState, project_hash: &str) -> Result<()> {
    let mut after: Option<(String, String)> = None;
    loop {
        let jobs = state.state_store.with_state_db(|db| {
            db.list_active_sync_jobs_by_operation_type_after(
                APPLY_OPERATION_TYPE,
                after
                    .as_ref()
                    .map(|(created_at, job_id)| (created_at.as_str(), job_id.as_str())),
                64,
            )
        })?;
        let Some(last) = jobs.last() else {
            return Ok(());
        };
        let next = (last.created_at.clone(), last.job_id.clone());
        for job in jobs {
            let operation = validate_job_operation(&job)?;
            if operation.project_hash == project_hash {
                anyhow::bail!(
                    "project apply {} requires recovery before another apply may begin",
                    operation.job_id
                );
            }
        }
        after = Some(next);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApplyRecoveryDecision {
    Commit,
    Rollback,
}

fn decide_apply_recovery(
    visible: Option<&str>,
    previous: Option<&str>,
    target: &str,
) -> Result<ApplyRecoveryDecision> {
    if visible == Some(target) {
        return Ok(ApplyRecoveryDecision::Commit);
    }
    if visible == previous {
        return Ok(ApplyRecoveryDecision::Rollback);
    }
    anyhow::bail!(
        "project apply recovery found unrelated deployed ref {visible:?}; expected {previous:?} or {target}"
    )
}

async fn recover_apply_job(state: &AppState, job: &SyncJobRecord) -> Result<()> {
    if job.state == SyncJobState::Running {
        anyhow::bail!("project apply recovery observed a running predecessor attempt");
    }
    let operation = validate_job_operation(job)?;
    let project_path = PathBuf::from(&operation.project_path);
    let project = PinnedDirectory::open(&project_path)?
        .context("project apply recovery cannot open its project root")?;
    project.ensure_path_binding()?;
    if project.identity()? != operation.project_identity {
        anyhow::bail!("project apply recovery project root identity changed");
    }
    let apply_lock = project_apply_lock(&operation.project_hash);
    let _apply_guard = apply_lock.lock_owned().await;
    let _project_guard = crate::project_namespace::acquire_project_mutation_lock(&project).await?;
    let _scheduler_guard = state.scheduler_runtime_gate.clone().write_owned().await;
    project.ensure_path_binding()?;
    let authority = state
        .state_store
        .with_state_db(|db| db.pinned_authority())?;
    let _cas_guard = authority.acquire_shared_guard()?;
    let cas = authority.cas_store()?;
    let expected_targets =
        expected_target_surface_identities_for_snapshot(&cas, &operation.target_snapshot_hash)?;
    let visible = state
        .state_store
        .with_state_db(|db| db.read_deployed_project_ref(&operation.project_hash))?
        .map(|head| head.target_hash);
    let transaction = open_transaction_root(&project, &operation.transaction_id)?;
    let journal = match transaction.as_ref() {
        Some((_, transaction, _))
            if transaction
                .entry_no_follow(OsStr::new("journal.json"))?
                .is_some() =>
        {
            let key = authority.require_recovery()?.workspace_journal_auth_key()?;
            let journal = read_apply_journal_from_transaction(transaction, &key)?;
            if journal.operation != operation {
                anyhow::bail!("project apply journal is bound to another operation");
            }
            verify_journal_target_identities(&journal.surfaces, &expected_targets)?;
            Some(journal)
        }
        _ => None,
    };
    let outcome = match decide_apply_recovery(
        visible.as_deref(),
        operation.previous_deployed_hash.as_deref(),
        &operation.target_snapshot_hash,
    )? {
        ApplyRecoveryDecision::Commit => {
            if transaction.is_some() && journal.is_none() {
                anyhow::bail!("committed project apply transaction has no authenticated journal");
            }
            verify_live_target_identities(&project, &expected_targets)?;
            project.ensure_path_binding()?;
            if let Some((parent, transaction, name)) = transaction.as_ref() {
                cleanup_transaction_root(parent, transaction, name.as_os_str())?;
            }
            project.ensure_path_binding()?;
            (SyncJobState::Completed, "recovered_committed", "committed")
        }
        ApplyRecoveryDecision::Rollback => {
            if let Some((parent, transaction, name)) = transaction.as_ref() {
                if let Some(journal) = journal.as_ref() {
                    if !live_journal_surfaces_match(&project, &journal.surfaces, false)? {
                        let workspace =
                            StagingDirectory::open(&project, &operation.transaction_id)?
                                .context("prepared project apply has an incomplete transaction")?;
                        rollback_journal_surfaces(&project, &workspace, &journal.surfaces)?;
                    }
                    // Schedule restoration is independently idempotent and
                    // remains required when a predecessor crashed after
                    // restoring the filesystem but before retiring the
                    // transaction.
                    project_deploy::restore_recovery_before_images(
                        state,
                        &journal.schedule_before_images,
                    )
                    .await?;
                    verify_live_journal_surfaces(&project, &journal.surfaces, false)?;
                    project.ensure_path_binding()?;
                    cleanup_transaction_root(parent, transaction, name.as_os_str())?;
                } else {
                    // Before journal publication no live surface or schedule
                    // may have changed. The transaction contains only
                    // disposable materialization bytes.
                    project.ensure_path_binding()?;
                    cleanup_transaction_root(parent, transaction, name.as_os_str())?;
                }
            }
            project.ensure_path_binding()?;
            (SyncJobState::Failed, "recovered_rolled_back", "rolled_back")
        }
    };
    settle_apply_job(
        state,
        &operation,
        outcome.0,
        outcome.1,
        (outcome.0 == SyncJobState::Failed)
            .then(|| "interrupted project apply was rolled back before startup".to_owned()),
        serde_json::json!({"outcome": outcome.2}),
        false,
    )
}

/// Recover every interrupted project materialization before scheduler or
/// thread dispatch becomes observable. The deployed ref selects commit versus
/// rollback; the pinned project identity, authenticated journal, and exact
/// snapshot-derived surface identities independently prove that the selected
/// resolution is safe. Any third ref value stops startup without guessing.
pub async fn recover_durable_project_snapshot_applies(state: &AppState) -> Result<usize> {
    let mut recovered = 0usize;
    loop {
        let jobs = state.state_store.with_state_db(|db| {
            db.list_active_sync_jobs_by_operation_type(APPLY_OPERATION_TYPE, 64)
        })?;
        if jobs.is_empty() {
            return Ok(recovered);
        }
        for job in jobs {
            recover_apply_job(state, &job).await?;
            recovered += 1;
        }
    }
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:project/apply-snapshot",
    endpoint: "project.apply-snapshot",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.project/apply-snapshot"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, ctx, state).await
        })
    },
};

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_operation(project: &Path, transaction_id: &str) -> ApplyOperation {
        let project_path = project.to_str().unwrap().to_owned();
        let project_identity = PinnedDirectory::open(project)
            .unwrap()
            .unwrap()
            .identity()
            .unwrap();
        ApplyOperation {
            operation_type: APPLY_OPERATION_TYPE.to_owned(),
            schema: APPLY_OPERATION_SCHEMA.to_owned(),
            job_id: format!("project-snapshot-apply:{transaction_id}"),
            attempt_id: format!("project-snapshot-apply-attempt:{transaction_id}"),
            transaction_id: transaction_id.to_owned(),
            project_hash: ryeos_state::refs::deployed_project_key(&project_path),
            project_identity,
            project_path,
            previous_deployed_hash: None,
            target_snapshot_hash: "a".repeat(64),
            caller_fingerprint: format!("fp:{}", "7".repeat(64)),
        }
    }

    #[test]
    fn recovery_decision_uses_only_the_exact_deployed_ref() {
        let target = "b".repeat(64);
        let previous = "a".repeat(64);
        assert_eq!(
            decide_apply_recovery(None, None, &target).unwrap(),
            ApplyRecoveryDecision::Rollback
        );
        assert_eq!(
            decide_apply_recovery(Some(&previous), Some(&previous), &target).unwrap(),
            ApplyRecoveryDecision::Rollback
        );
        assert_eq!(
            decide_apply_recovery(Some(&target), Some(&previous), &target).unwrap(),
            ApplyRecoveryDecision::Commit
        );
        assert!(decide_apply_recovery(Some(&"c".repeat(64)), Some(&previous), &target).is_err());
    }

    #[test]
    fn operation_coordinates_and_project_path_must_be_canonical() {
        let project = TempDir::new().unwrap();
        let project_path = project.path().canonicalize().unwrap();
        let mut operation = test_operation(&project_path, "ad49cb84-a1b7-49b8-aab1-a66f27b4eb68");
        assert!(validate_apply_operation(&operation).is_ok());
        operation.transaction_id = "../../outside".to_owned();
        operation.job_id = "project-snapshot-apply:../../outside".to_owned();
        operation.attempt_id = "project-snapshot-apply-attempt:../../outside".to_owned();
        assert!(validate_apply_operation(&operation).is_err());

        for invalid in [
            "/tmp/../etc",
            "/tmp/./project",
            "/tmp//project",
            "/tmp/project/",
            "/",
        ] {
            let mut operation =
                test_operation(&project_path, "6d180259-f3e0-4551-b13b-d95a8b384080");
            operation.project_path = invalid.to_owned();
            operation.project_hash =
                ryeos_state::refs::deployed_project_key(&operation.project_path);
            assert!(validate_apply_operation(&operation).is_err(), "{invalid}");
        }

        let operation = test_operation(&project_path, "b0e32f74-a8e3-4b13-89da-fc97469e9a40");
        let mut value = serde_json::to_value(&operation).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .remove("previous_deployed_hash");
        assert!(serde_json::from_value::<ApplyOperation>(value).is_err());

        let mut operation = operation;
        operation.target_snapshot_hash = "A".repeat(64);
        assert!(validate_apply_operation(&operation).is_err());
    }

    #[test]
    fn authenticated_journal_rejects_modified_schedule_before_image() {
        let project = TempDir::new().unwrap();
        let project_path = project.path().canonicalize().unwrap();
        let project_directory = PinnedDirectory::open(&project_path).unwrap().unwrap();
        let transaction_id = "3e5e31a4-5955-41cc-8c9d-32777ed87622";
        let workspace = StagingDirectory::create(&project_directory, transaction_id).unwrap();
        let key = [7_u8; 32];
        write_apply_journal(
            &workspace,
            &key,
            ApplyJournal {
                kind: APPLY_JOURNAL_KIND.to_owned(),
                schema: APPLY_JOURNAL_SCHEMA,
                operation: test_operation(&project_path, transaction_id),
                surfaces: ryeos_state::project_sync::materialized_project_ai_surfaces()
                    .map(|surface| JournalSurface {
                        relative: surface.root.to_owned(),
                        prior_content: None,
                        target_content: None,
                    })
                    .collect(),
                schedule_before_images: vec![
                    crate::project_deploy::schedules::ScheduleRecoveryBeforeImage {
                        schedule_id: "test-schedule".to_owned(),
                        signed_yaml: Some("signed: before\n".to_owned()),
                    },
                ],
                auth_tag: String::new(),
            },
        )
        .unwrap();
        assert_eq!(
            read_apply_journal(&workspace, &key)
                .unwrap()
                .schedule_before_images[0]
                .signed_yaml
                .as_deref(),
            Some("signed: before\n")
        );
        let path = workspace.transaction.path().join("journal.json");
        let bytes = std::fs::read(&path).unwrap();
        let mut value: Value = serde_json::from_slice(&bytes).unwrap();
        value["auth_tag"] = Value::String(value["auth_tag"].as_str().unwrap().to_ascii_uppercase());
        std::fs::write(&path, lillux::canonical_json(&value).unwrap()).unwrap();
        assert!(read_apply_journal(&workspace, &key).is_err());

        let mut value: Value = serde_json::from_slice(&bytes).unwrap();
        value["schedule_before_images"][0]["signed_yaml"] =
            Value::String("signed: changed\n".to_owned());
        std::fs::write(&path, lillux::canonical_json(&value).unwrap()).unwrap();
        assert!(read_apply_journal(&workspace, &key).is_err());
    }

    fn apply_surface_prefix(
        project: &PinnedDirectory,
        workspace: &StagingDirectory,
        surfaces: &[JournalSurface],
        count: usize,
    ) {
        let declared =
            ryeos_state::project_sync::materialized_project_ai_surfaces().collect::<Vec<_>>();
        for (surface, frozen) in declared.into_iter().zip(surfaces).take(count) {
            if frozen.prior_content.is_some() {
                let (live_parent, name) =
                    crate::project_namespace::relative_parent(project, surface.root, false)
                        .unwrap()
                        .unwrap();
                let (backup_parent, _) = crate::project_namespace::relative_parent(
                    &workspace.backup,
                    surface.root,
                    true,
                )
                .unwrap()
                .unwrap();
                move_exact_child(&live_parent, &backup_parent, &name).unwrap();
            }
            if frozen.target_content.is_some() {
                let (staged_parent, name) = crate::project_namespace::relative_parent(
                    staging_directory(workspace),
                    surface.root,
                    false,
                )
                .unwrap()
                .unwrap();
                let (live_parent, _) = crate::project_namespace::relative_parent_with_mode(
                    project,
                    surface.root,
                    true,
                    0o755,
                )
                .unwrap()
                .unwrap();
                move_exact_child(&staged_parent, &live_parent, &name).unwrap();
            }
        }
    }

    fn staging_directory(workspace: &StagingDirectory) -> &PinnedDirectory {
        workspace.directory()
    }

    #[test]
    fn rollback_rejects_modified_authenticated_prior_content() {
        let project = TempDir::new().unwrap();
        std::fs::create_dir_all(project.path().join(".ai")).unwrap();
        std::fs::write(project.path().join(".ai/manifest.yaml"), "old-file").unwrap();
        let project_path = project.path().canonicalize().unwrap();
        let project_directory = PinnedDirectory::open(&project_path).unwrap().unwrap();
        let workspace =
            StagingDirectory::create(&project_directory, "d386534f-16ed-45ca-9a42-214b62c7466f")
                .unwrap();
        std::fs::create_dir_all(workspace.directory.path().join(".ai")).unwrap();
        std::fs::write(
            workspace.directory.path().join(".ai/manifest.yaml"),
            "new-file",
        )
        .unwrap();
        let surfaces = observe_journal_surfaces(&project_directory, workspace.directory()).unwrap();
        apply_surface_prefix(&project_directory, &workspace, &surfaces, 2);
        std::fs::write(
            workspace.backup.path().join(".ai/manifest.yaml"),
            "changed-old-file",
        )
        .unwrap();
        assert!(rollback_journal_surfaces(&project_directory, &workspace, &surfaces).is_err());
    }

    #[test]
    fn every_surface_phase_prefix_rolls_back_files_and_directories_and_allows_retry() {
        let surface_count = ryeos_state::project_sync::materialized_project_ai_surfaces().count();
        for prefix in 0..=surface_count {
            let project = TempDir::new().unwrap();
            std::fs::create_dir_all(project.path().join(".ai/directives")).unwrap();
            std::fs::write(project.path().join(".ai/manifest.yaml"), "old-file").unwrap();
            std::fs::write(project.path().join(".ai/directives/old.md"), "old-dir").unwrap();
            let project_path = project.path().canonicalize().unwrap();
            let project_directory = PinnedDirectory::open(&project_path).unwrap().unwrap();
            let transaction_id = format!("00000000-0000-4000-8000-{prefix:012x}");
            let workspace = StagingDirectory::create(&project_directory, &transaction_id).unwrap();
            std::fs::create_dir_all(workspace.directory.path().join(".ai/directives")).unwrap();
            std::fs::write(
                workspace.directory.path().join(".ai/manifest.yaml"),
                "new-file",
            )
            .unwrap();
            std::fs::write(
                workspace.directory.path().join(".ai/directives/new.md"),
                "new-dir",
            )
            .unwrap();
            let surfaces =
                observe_journal_surfaces(&project_directory, workspace.directory()).unwrap();
            apply_surface_prefix(&project_directory, &workspace, &surfaces, prefix);
            rollback_journal_surfaces(&project_directory, &workspace, &surfaces).unwrap();
            assert_eq!(
                std::fs::read_to_string(project.path().join(".ai/manifest.yaml")).unwrap(),
                "old-file"
            );
            assert!(project.path().join(".ai/directives/old.md").exists());
            assert!(!project.path().join(".ai/directives/new.md").exists());
            workspace.cleanup().unwrap();
            let retry_id = format!("10000000-0000-4000-8000-{prefix:012x}");
            let retry = StagingDirectory::create(&project_directory, &retry_id).unwrap();
            retry.cleanup().unwrap();
        }
    }

    #[test]
    fn replace_managed_surfaces_handles_files_and_directories() {
        let project = TempDir::new().unwrap();
        std::fs::create_dir_all(project.path().join(".ai/directives")).unwrap();
        std::fs::create_dir_all(project.path().join(".ai/tools")).unwrap();
        std::fs::create_dir_all(project.path().join(".ai/config/schedules")).unwrap();
        std::fs::create_dir_all(project.path().join(".ai/node/schedules")).unwrap();
        std::fs::create_dir_all(project.path().join("src")).unwrap();
        std::fs::write(
            project.path().join(".ai/manifest.source.yaml"),
            "old source",
        )
        .unwrap();
        std::fs::write(project.path().join(".ai/manifest.yaml"), "old generated").unwrap();
        std::fs::write(project.path().join(".ai/directives/old.md"), "old").unwrap();
        std::fs::write(project.path().join(".ai/tools/old.sh"), "old").unwrap();
        std::fs::write(project.path().join(".ai/config/schedules/old.yaml"), "old").unwrap();
        std::fs::write(
            project.path().join(".ai/node/schedules/runtime.yaml"),
            "runtime",
        )
        .unwrap();
        std::fs::write(project.path().join("src/index.ts"), "app").unwrap();

        let project_root = project.path().canonicalize().unwrap();
        let project_directory = PinnedDirectory::open(&project_root).unwrap().unwrap();
        let staging = StagingDirectory::create(&project_directory, "test-apply-one").unwrap();
        std::fs::create_dir_all(staging.directory.path().join(".ai/directives")).unwrap();
        std::fs::create_dir_all(staging.directory.path().join(".ai/config/schedules")).unwrap();
        std::fs::create_dir_all(staging.directory.path().join(".ai")).unwrap();
        std::fs::write(
            staging.directory.path().join(".ai/manifest.source.yaml"),
            "new source",
        )
        .unwrap();
        std::fs::write(
            staging.directory.path().join(".ai/directives/new.md"),
            "new",
        )
        .unwrap();
        std::fs::write(
            staging
                .directory
                .path()
                .join(".ai/config/schedules/new.yaml"),
            "new",
        )
        .unwrap();

        let surfaces = observe_journal_surfaces(&project_directory, staging.directory()).unwrap();
        let mut prepared =
            replace_managed_surfaces(&project_directory, &staging, &surfaces, 3).unwrap();
        prepared.finalize();
        let report = prepared.report.clone();
        assert!(report.surfaces_replaced >= 1);
        assert!(report.surfaces_deleted >= 1);
        assert_eq!(
            std::fs::read_to_string(project.path().join(".ai/manifest.source.yaml")).unwrap(),
            "new source"
        );
        assert!(!project.path().join(".ai/manifest.yaml").exists());
        assert!(project.path().join(".ai/directives/new.md").exists());
        assert!(!project.path().join(".ai/directives/old.md").exists());
        assert!(!project.path().join(".ai/tools").exists());
        assert!(
            project
                .path()
                .join(".ai/config/schedules/new.yaml")
                .exists()
        );
        assert!(
            !project
                .path()
                .join(".ai/config/schedules/old.yaml")
                .exists()
        );
        assert_eq!(
            std::fs::read_to_string(project.path().join(".ai/node/schedules/runtime.yaml"))
                .unwrap(),
            "runtime"
        );
        assert_eq!(
            std::fs::read_to_string(project.path().join("src/index.ts")).unwrap(),
            "app"
        );
    }

    #[test]
    fn replace_managed_surfaces_rejects_symlinked_live_directory() {
        #[cfg(not(unix))]
        {
            return;
        }

        let project = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        std::fs::create_dir_all(project.path().join(".ai")).unwrap();
        std::os::unix::fs::symlink(outside.path(), project.path().join(".ai/directives")).unwrap();

        let project_root = project.path().canonicalize().unwrap();
        let project_directory = PinnedDirectory::open(&project_root).unwrap().unwrap();
        let staging = StagingDirectory::create(&project_directory, "test-apply-two").unwrap();
        std::fs::create_dir_all(staging.directory.path().join(".ai/directives")).unwrap();
        std::fs::write(
            staging.directory.path().join(".ai/directives/new.md"),
            "new",
        )
        .unwrap();
        let err = observe_journal_surfaces(&project_directory, staging.directory())
            .expect_err("symlink surface must be rejected");
        assert!(format!("{err:#}").contains("not a directory"), "{err:#}");
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
        assert!(
            std::fs::symlink_metadata(project.path().join(".ai/directives"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn replace_managed_surfaces_rejects_symlinked_live_file() {
        #[cfg(not(unix))]
        {
            return;
        }

        let project = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        std::fs::create_dir_all(project.path().join(".ai")).unwrap();
        std::fs::write(outside.path().join("manifest.yaml"), "outside").unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("manifest.yaml"),
            project.path().join(".ai/manifest.yaml"),
        )
        .unwrap();

        let project_root = project.path().canonicalize().unwrap();
        let project_directory = PinnedDirectory::open(&project_root).unwrap().unwrap();
        let staging = StagingDirectory::create(&project_directory, "test-apply-three").unwrap();
        std::fs::create_dir_all(staging.directory.path().join(".ai")).unwrap();
        std::fs::write(staging.directory.path().join(".ai/manifest.yaml"), "new").unwrap();
        let err = observe_journal_surfaces(&project_directory, staging.directory())
            .expect_err("symlinked file surface must be rejected");
        assert!(format!("{err:#}").contains("not a regular file"), "{err:#}");
        assert_eq!(
            std::fs::read_to_string(outside.path().join("manifest.yaml")).unwrap(),
            "outside"
        );
        assert!(
            std::fs::symlink_metadata(project.path().join(".ai/manifest.yaml"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn materialize_tree_restores_executable_mode() {
        #[cfg(not(unix))]
        {
            return;
        }

        use std::os::unix::fs::PermissionsExt;

        let cas_dir = TempDir::new().unwrap();
        let cas = CasStore::new(cas_dir.path().to_path_buf());
        let bytes = b"#!/bin/sh\n";
        let blob_hash = cas.store_blob(bytes).unwrap();
        let file = ProjectFile {
            blob_hash,
            size: bytes.len() as u64,
            normalized_mode: 0o755,
        };
        let file_hash = cas.store_object(&file.to_value()).unwrap();
        let mut map = std::collections::BTreeMap::new();
        map.insert(".ai/tools/run.sh".to_string(), file_hash);
        let tree = ProjectTree { files: map };
        let staging = TempDir::new().unwrap();

        let staging_root = staging.path().canonicalize().unwrap();
        let staging_directory = PinnedDirectory::open(&staging_root).unwrap().unwrap();
        materialize_tree_to_staging(&cas, &tree, &staging_directory).unwrap();
        let mode = std::fs::metadata(staging.path().join(".ai/tools/run.sh"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o755);
    }

    #[test]
    fn apply_mode_clamps_special_bits() {
        #[cfg(not(unix))]
        {
            return;
        }

        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new().unwrap();
        let path = dir.path().join("file");
        std::fs::write(&path, "content").unwrap();

        apply_mode(&path, Some(0o4755)).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o755);
    }

    #[test]
    fn preflight_schedule_declarations_accepts_valid_project_intent() {
        let project = TempDir::new().unwrap();
        let staging = TempDir::new().unwrap();
        let schedules = staging.path().join(".ai/config/schedules");
        std::fs::create_dir_all(&schedules).unwrap();
        std::fs::write(
            schedules.join("snap-track.yaml"),
            format!(
                r#"category: schedules
version: 1.0.0
schema_version: 1.0.0
schedules:
  - schedule_id: snap-track-discover-feed-scrape
    item_ref: graph:snap-track/discover_feed_scrape
    ref_bindings: {{}}
    schedule_type: cron
    expression: "0 */15 * * * *"
    timezone: UTC
    misfire_policy: skip
    overlap_policy: skip
    lateness_grace_secs: 60
    enabled: true
    capabilities:
      - ryeos.execute.graph.snap-track/discover_feed_scrape
    execution_policy:
      schema_version: 2
      ownership: daemon_owned
      recovery: restart_recoverable
      response: accepted
      target:
        kind: here
      environment:
        kind: project_overlay
        include_operator_vault: false
        name_policy:
          kind: declared_required
      project:
        kind: live_direct
        access: read_write
        child_policy:
          kind: inherit
    project_root: {}
    params:
      country: US
"#,
                project.path().display()
            ),
        )
        .unwrap();

        let count = crate::project_deploy::schedules::validate_declarations_for_test(
            staging.path(),
            project.path(),
        )
        .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn preflight_schedule_declarations_rejects_duplicate_ids() {
        let project = TempDir::new().unwrap();
        let staging = TempDir::new().unwrap();
        let schedules = staging.path().join(".ai/config/schedules");
        std::fs::create_dir_all(&schedules).unwrap();
        for file in ["a.yaml", "b.yaml"] {
            std::fs::write(
                schedules.join(file),
                r#"category: schedules
version: 1.0.0
schema_version: 1.0.0
schedules:
  - schedule_id: duplicate-schedule
    item_ref: graph:snap-track/discover_feed_scrape
    ref_bindings: {}
    schedule_type: cron
    expression: "0 */15 * * * *"
    timezone: UTC
    misfire_policy: skip
    overlap_policy: skip
    lateness_grace_secs: 60
    enabled: true
    capabilities:
      - ryeos.execute.graph.snap-track/discover_feed_scrape
    execution_policy:
      schema_version: 2
      ownership: daemon_owned
      recovery: restart_recoverable
      response: accepted
      target:
        kind: here
      environment:
        kind: project_overlay
        include_operator_vault: false
        name_policy:
          kind: declared_required
      project:
        kind: live_direct
        access: read_write
        child_policy:
          kind: inherit
    params: {}
"#,
            )
            .unwrap();
        }

        let err = crate::project_deploy::schedules::validate_declarations_for_test(
            staging.path(),
            project.path(),
        )
        .expect_err("duplicate schedule ids must fail preflight");
        assert!(format!("{err:#}").contains("duplicate schedule_id"));
    }

    #[test]
    fn preflight_schedule_declarations_rejects_node_owned_fields() {
        let project = TempDir::new().unwrap();
        let staging = TempDir::new().unwrap();
        let schedules = staging.path().join(".ai/config/schedules");
        std::fs::create_dir_all(&schedules).unwrap();
        std::fs::write(
            schedules.join("bad.yaml"),
            r#"category: schedules
version: 1.0.0
schema_version: 1.0.0
schedules:
  - schedule_id: bad-schedule
    item_ref: graph:snap-track/discover_feed_scrape
    ref_bindings: {}
    schedule_type: cron
    expression: "0 */15 * * * *"
    execution:
      requester_fingerprint: fp:test
"#,
        )
        .unwrap();

        let err = crate::project_deploy::schedules::validate_declarations_for_test(
            staging.path(),
            project.path(),
        )
        .expect_err("node-owned execution field must fail preflight");
        assert!(format!("{err:#}").contains("unknown field `execution`"));
    }

    #[test]
    fn preflight_schedule_declarations_rejects_other_project_root() {
        let project = TempDir::new().unwrap();
        let other_project = TempDir::new().unwrap();
        let staging = TempDir::new().unwrap();
        let schedules = staging.path().join(".ai/config/schedules");
        std::fs::create_dir_all(&schedules).unwrap();
        std::fs::write(
            schedules.join("bad-root.yaml"),
            format!(
                r#"category: schedules
version: 1.0.0
schema_version: 1.0.0
schedules:
  - schedule_id: wrong-root
    item_ref: graph:snap-track/discover_feed_scrape
    ref_bindings: {{}}
    schedule_type: cron
    expression: "0 */15 * * * *"
    timezone: UTC
    misfire_policy: skip
    overlap_policy: skip
    lateness_grace_secs: 60
    enabled: true
    capabilities:
      - ryeos.execute.graph.snap-track/discover_feed_scrape
    execution_policy:
      schema_version: 2
      ownership: daemon_owned
      recovery: restart_recoverable
      response: accepted
      target:
        kind: here
      environment:
        kind: project_overlay
        include_operator_vault: false
        name_policy:
          kind: declared_required
      project:
        kind: live_direct
        access: read_write
        child_policy:
          kind: inherit
    params: {{}}
    project_root: {}
"#,
                other_project.path().display()
            ),
        )
        .unwrap();

        let err = crate::project_deploy::schedules::validate_declarations_for_test(
            staging.path(),
            project.path(),
        )
        .expect_err("foreign project root must fail preflight");
        assert!(format!("{err:#}").contains("cannot target another project"));
    }

    #[test]
    fn preflight_schedule_declarations_rejects_projectless_execution() {
        let project = TempDir::new().unwrap();
        let staging = TempDir::new().unwrap();
        let schedules = staging.path().join(".ai/config/schedules");
        std::fs::create_dir_all(&schedules).unwrap();
        std::fs::write(
            schedules.join("projectless.yaml"),
            r#"category: schedules
version: 1.0.0
schema_version: 1.0.0
schedules:
  - schedule_id: projectless-from-project
    item_ref: service:operator/task
    ref_bindings: {}
    schedule_type: cron
    expression: "0 */15 * * * *"
    timezone: UTC
    misfire_policy: skip
    overlap_policy: skip
    lateness_grace_secs: 60
    enabled: true
    capabilities:
      - ryeos.execute.service.operator/task
    execution_policy:
      schema_version: 2
      ownership: daemon_owned
      recovery: restart_recoverable
      response: accepted
      target:
        kind: here
      environment:
        kind: none
      project:
        kind: projectless
    params: {}
"#,
        )
        .unwrap();

        let err = crate::project_deploy::schedules::validate_declarations_for_test(
            staging.path(),
            project.path(),
        )
        .expect_err("project-managed schedules must retain project authority");
        assert!(format!("{err:#}").contains("must use a project-backed execution policy"));
    }
}
