use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use ryeos_node_runtime::node_document;
use ryeos_scheduler::types::ScheduleSpecRecord;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::retention::SchedulerService;
use ryeos_scheduler_contract::{
    ProjectScheduleCaller, ProjectScheduleDeployParticipant, ProjectScheduleDeployRequest,
    ProjectScheduleFuture, ProjectScheduleSession, ScheduleDeployReport,
    ScheduleRecoveryBeforeImage,
};
use tokio::sync::{OwnedRwLockWriteGuard, RwLock};

const MANAGED_BY_TYPE: &str = "project_ai_sync";
const LOCKED_SCAN_REFUSAL: &str =
    "project schedule directory is not canonical under the deploy lock";
const MAX_RECOVERY_SCHEDULE_SOURCE_BYTES: u64 = 1024 * 1024;
const SCHEDULE_DIRECTORY_LOCK_TIMEOUT: lillux::time::Duration =
    lillux::time::Duration::from_secs(5);

fn project_path_identity(path: &Path) -> Result<&str> {
    path.to_str().ok_or_else(|| {
        anyhow!(
            "canonical project path '{}' is not valid UTF-8 and cannot be used as schedule authority",
            path.display()
        )
    })
}

#[derive(Debug, Default)]
pub struct ScheduleDeployPlan {
    actions: Vec<ScheduleAction>,
    declared: usize,
}

struct ProjectContext {
    request: Arc<ProjectScheduleDeployRequest>,
    app_root: PathBuf,
    db: Arc<ScheduleDb>,
    trust_store: Arc<ryeos_engine::trust::TrustStore>,
    identity: Arc<ryeos_node_runtime::identity::NodeIdentity>,
    reload_tx: Option<tokio::sync::mpsc::Sender<ryeos_scheduler::ReloadSignal>>,
}

type ScheduleDb = ryeos_scheduler::db::SchedulerDb;

#[derive(Debug)]
enum ScheduleAction {
    Create(DesiredSchedule),
    Update {
        desired: DesiredSchedule,
        /// Boxed: the full spec record dwarfs `Create`'s payload.
        existing: Box<ScheduleSpecRecord>,
        adopt_manual: bool,
    },
    DeleteMissing {
        schedule_id: String,
        existing: Box<ScheduleSpecRecord>,
    },
}

/// Exact signed node-schedule source present before one project apply.
/// The source document, not a reconstructed record, is the durable before
/// image because it is what owns the scheduler projection.
/// Recovery preparation retains the exact node schedule-directory lock from
/// before-image capture through the eventual projection mutation. A caller
/// must not split those phases and reopen the directory by pathname: doing so
/// would let a replacement become the durable rollback authority.
pub(crate) struct ScheduleRecoveryPreparation {
    before_images: Vec<ScheduleRecoveryBeforeImage>,
    tx: ScheduleReconcileTx,
}

impl ScheduleRecoveryPreparation {
    pub(crate) fn before_images(&self) -> &[ScheduleRecoveryBeforeImage] {
        &self.before_images
    }
}

pub(crate) async fn prepare_recovery_before_images(
    plan: &ScheduleDeployPlan,
    ctx: &ProjectContext,
) -> Result<ScheduleRecoveryPreparation> {
    let schedules_dir = ctx
        .app_root
        .join(ryeos_engine::AI_DIR)
        .join("node/schedules");
    let schedule_ids = recovery_schedule_ids(plan)
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    tokio::task::spawn_blocking(move || {
        let tx =
            ScheduleReconcileTx::new_with_timeout(&schedules_dir, SCHEDULE_DIRECTORY_LOCK_TIMEOUT)?;
        let before_images = schedule_ids
            .into_iter()
            .map(|schedule_id| {
                let name = format!("{schedule_id}.yaml");
                let signed_yaml = tx
                    .directory
                    .open_pinned_regular(std::ffi::OsStr::new(&name), false)?
                    .map(|file| {
                        String::from_utf8(file.read_bounded(MAX_RECOVERY_SCHEDULE_SOURCE_BYTES)?)
                            .context("signed schedule before-image is not UTF-8")
                    })
                    .transpose()?;
                Ok(ScheduleRecoveryBeforeImage {
                    schedule_id,
                    signed_yaml,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(ScheduleRecoveryPreparation { before_images, tx })
    })
    .await
    .context("join schedule recovery preparation")?
}

fn recovery_schedule_ids(plan: &ScheduleDeployPlan) -> Vec<&str> {
    let mut ids = plan
        .actions
        .iter()
        .map(|action| match action {
            ScheduleAction::Create(desired) | ScheduleAction::Update { desired, .. } => {
                desired.declaration.schedule_id.as_str()
            }
            ScheduleAction::DeleteMissing { schedule_id, .. } => schedule_id.as_str(),
        })
        .collect::<Vec<_>>();
    ids.sort_unstable();
    ids.dedup();
    ids
}

fn restore_recovery_sources_and_projection(
    directory: &lillux::PinnedDirectory,
    scheduler_db: &ryeos_scheduler::db::SchedulerDb,
    trust_store: &ryeos_engine::trust::TrustStore,
    entries: &[ScheduleRecoveryBeforeImage],
) -> Result<HashSet<String>> {
    let mut touched = HashSet::new();
    for entry in entries.iter().rev() {
        let name = format!("{}.yaml", entry.schedule_id);
        let name = std::ffi::OsStr::new(&name);
        let expected = directory.open_pinned_regular(name, false)?;
        match &entry.signed_yaml {
            Some(content) => {
                if content.len() as u64 > MAX_RECOVERY_SCHEDULE_SOURCE_BYTES {
                    anyhow::bail!("schedule recovery source exceeds its byte limit");
                }
                directory.atomic_write_pinned_if_same(
                    name,
                    expected.as_ref(),
                    content.as_bytes(),
                    0o600,
                    crate::SCHEDULER_TEMP_PURPOSE,
                )?;
                let path = directory.path().join(name);
                let verified = ryeos_scheduler::projection::verify_schedule_source_content(
                    &path,
                    content,
                    trust_store,
                )?;
                scheduler_db.upsert_spec(&verified.to_spec_record()?)?;
            }
            None => {
                if let Some(file) = expected {
                    directory.remove_pinned_regular_if_same(&file)?;
                }
                scheduler_db.delete_spec(&entry.schedule_id)?;
            }
        }
        touched.insert(entry.schedule_id.clone());
    }
    Ok(touched)
}

#[derive(Debug, Clone)]
struct DesiredSchedule {
    declaration: ScheduleDeclaration,
    source_path: String,
    source_body_hash: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScheduleDeclarationFile {
    category: String,
    version: String,
    schema_version: String,
    #[serde(default)]
    #[serde(rename = "description")]
    _description: Option<String>,
    schedules: Vec<ScheduleDeclaration>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScheduleDeclaration {
    schedule_id: String,
    item_ref: String,
    ref_bindings: BTreeMap<String, String>,
    schedule_type: String,
    expression: String,
    timezone: String,
    misfire_policy: String,
    overlap_policy: String,
    lateness_grace_secs: i64,
    enabled: bool,
    capabilities: Vec<String>,
    #[serde(default)]
    project_root: Option<String>,
    params: Value,
    execution_policy: ryeos_engine::execution_contract::ExecutionPolicy,
}

#[cfg(test)]
pub(crate) fn validate_declarations_for_test(
    staging_root: &Path,
    project_path: &Path,
) -> Result<usize> {
    Ok(load_desired_schedules(staging_root, project_path)?.len())
}

pub fn plan(ctx: &ProjectContext) -> Result<ScheduleDeployPlan> {
    let desired = load_desired_schedules(
        ctx.request.staging_root.as_path(),
        ctx.request.project_path.as_path(),
    )?;
    let canonical_project_path = project_path_identity(ctx.request.project_path.as_path())?;
    let mut actions = Vec::new();
    let desired_ids: HashSet<String> = desired
        .iter()
        .map(|schedule| schedule.declaration.schedule_id.clone())
        .collect();
    let desired_by_id: HashMap<String, DesiredSchedule> = desired
        .into_iter()
        .map(|schedule| (schedule.declaration.schedule_id.clone(), schedule))
        .collect();

    let node_dir = ctx.app_root.join(ryeos_engine::AI_DIR).join("node");
    let schedules_dir = node_dir.join("schedules");
    let managed_yaml = load_project_managed_schedule_yaml(
        &schedules_dir,
        ctx.request.project_key.as_str(),
        &ctx.trust_store,
    )?;

    for desired_schedule in desired_by_id.values() {
        let schedule_id = &desired_schedule.declaration.schedule_id;
        let existing = ctx.db.get_spec(schedule_id)?;
        let existing_body =
            read_existing_schedule_body(&schedules_dir, schedule_id, &ctx.trust_store)?;

        match existing {
            Some(existing) => {
                let adopt_manual = match existing_body.as_ref().and_then(parse_project_managed_by) {
                    Some(managed) => {
                        if managed.project_key != ctx.request.project_key.as_str() {
                            anyhow::bail!(
                                "schedule_id '{}' is managed by another project and cannot be updated by this project sync",
                                schedule_id
                            );
                        }
                        false
                    }
                    None => {
                        let existing_project_root = existing.project_root.as_deref();
                        if existing_project_root != Some(canonical_project_path) {
                            anyhow::bail!(
                                "schedule_id '{}' already exists as a manual schedule for project_root {:?}; refusing to adopt for project {}",
                                schedule_id,
                                existing_project_root,
                                ctx.request.project_path.as_path().display(),
                            );
                        }
                        true
                    }
                };
                require_project_reconcile_schedule_owner(
                    ctx.request.caller.as_ref(),
                    schedule_id,
                    existing.execution.principal_id(),
                )?;
                actions.push(ScheduleAction::Update {
                    desired: desired_schedule.clone(),
                    existing: Box::new(existing),
                    adopt_manual,
                });
            }
            None => {
                if existing_body.is_some() {
                    anyhow::bail!(
                        "schedule_id '{}' has node YAML but no DB projection; rebuild projection or deregister before project sync",
                        schedule_id
                    );
                }
                require_schedule_registration_authority(ctx)?;
                reject_schedule_history_reuse(ctx, schedule_id)?;
                actions.push(ScheduleAction::Create(desired_schedule.clone()));
            }
        }
    }

    for schedule_id in managed_yaml.keys() {
        if desired_ids.contains(schedule_id) {
            continue;
        }
        let existing = ctx.db.get_spec(schedule_id)?.ok_or_else(|| {
            anyhow!(
                "project-managed schedule YAML '{}' exists without DB projection; rebuild projection or deregister before project sync",
                schedule_id
            )
        })?;
        require_project_reconcile_schedule_owner(
            ctx.request.caller.as_ref(),
            schedule_id,
            existing.execution.principal_id(),
        )?;
        actions.push(ScheduleAction::DeleteMissing {
            schedule_id: schedule_id.clone(),
            existing: Box::new(existing),
        });
    }

    for existing in ctx.db.list_specs(false, None)? {
        if desired_ids.contains(&existing.schedule_id)
            || managed_yaml.contains_key(&existing.schedule_id)
        {
            continue;
        }
        if existing.project_root.as_deref() == Some(canonical_project_path) {
            let yaml_path = schedules_dir.join(format!("{}.yaml", existing.schedule_id));
            if !yaml_path.exists() {
                anyhow::bail!(
                    "schedule_id '{}' has DB projection for this project but no node YAML; rebuild projection or deregister before project sync",
                    existing.schedule_id
                );
            }
        }
    }

    Ok(ScheduleDeployPlan {
        actions,
        declared: desired_ids.len(),
    })
}

fn prepare_commit_with_recovery(
    plan: &ScheduleDeployPlan,
    ctx: &ProjectContext,
    preparation: &mut ScheduleRecoveryPreparation,
) -> Result<ScheduleDeployReport> {
    let expected_ids = recovery_schedule_ids(plan);
    if expected_ids.len() != preparation.before_images.len()
        || expected_ids
            .iter()
            .zip(&preparation.before_images)
            .any(|(expected, observed)| *expected != observed.schedule_id.as_str())
    {
        anyhow::bail!("schedule recovery preparation does not match its deploy plan");
    }
    let ScheduleRecoveryPreparation { before_images, tx } = preparation;
    apply_actions(plan, ctx, tx, before_images)
}

fn apply_actions(
    plan: &ScheduleDeployPlan,
    ctx: &ProjectContext,
    tx: &mut ScheduleReconcileTx,
    before_images: &[ScheduleRecoveryBeforeImage],
) -> Result<ScheduleDeployReport> {
    let mut report = ScheduleDeployReport {
        declared: plan.declared,
        ..ScheduleDeployReport::default()
    };
    // Under the held lock the whole directory must still be canonical schedule
    // sources: no stray entries and no invalid schedule ids.
    ryeos_scheduler::projection::canonical_schedule_source_paths(tx.directory.path())
        .context(LOCKED_SCAN_REFUSAL)?;

    for action in &plan.actions {
        // The verified incumbent is the exact pinned inode whose bytes were
        // checked; the backup and the conditional write both bind to it.
        let incumbent = revalidate_action(action, ctx, &tx.directory)?;
        require_incumbent_matches_before_image(action, incumbent.as_ref(), before_images)?;
        match action {
            ScheduleAction::Create(desired) => {
                tx.backup(ctx, &desired.declaration.schedule_id, incumbent.as_ref())?;
                write_reconciled_schedule(
                    &tx.directory,
                    incumbent.as_ref().map(|incumbent| &incumbent.file),
                    desired,
                    ctx,
                    lillux::time::timestamp_millis(),
                )?;
                tx.touch(desired.declaration.schedule_id.clone());
                report.created += 1;
            }
            ScheduleAction::Update {
                desired,
                existing,
                adopt_manual: _,
            } => {
                tx.backup(ctx, &desired.declaration.schedule_id, incumbent.as_ref())?;
                write_reconciled_schedule(
                    &tx.directory,
                    incumbent.as_ref().map(|incumbent| &incumbent.file),
                    desired,
                    ctx,
                    existing.registered_at,
                )?;
                tx.touch(desired.declaration.schedule_id.clone());
                report.updated += 1;
            }
            ScheduleAction::DeleteMissing {
                schedule_id,
                existing: _,
            } => {
                let incumbent = incumbent.as_ref().ok_or_else(|| {
                    anyhow!("schedule_id '{schedule_id}' delete has no verified source")
                })?;
                tx.backup(ctx, schedule_id, Some(incumbent))?;
                tx.directory
                    .remove_pinned_regular_if_same(&incumbent.file)
                    .with_context(|| {
                        format!(
                            "delete schedule YAML {}",
                            tx.schedule_path(schedule_id).display()
                        )
                    })?;
                ctx.db.delete_spec(schedule_id)?;
                tx.touch(schedule_id.clone());
                report.deleted += 1;
            }
        }
    }
    Ok(report)
}

/// In-process rollback restores the backup bytes and journal recovery restores
/// the captured before-image. Both must be the same bytes, so a source that was
/// replaced after capture is refused even when its signed body is unchanged.
fn require_incumbent_matches_before_image(
    action: &ScheduleAction,
    incumbent: Option<&VerifiedIncumbent>,
    before_images: &[ScheduleRecoveryBeforeImage],
) -> Result<()> {
    let schedule_id = match action {
        ScheduleAction::Create(desired)
        | ScheduleAction::Update {
            desired,
            existing: _,
            adopt_manual: _,
        } => desired.declaration.schedule_id.as_str(),
        ScheduleAction::DeleteMissing {
            schedule_id,
            existing: _,
        } => schedule_id.as_str(),
    };
    let image = before_images
        .iter()
        .find(|image| image.schedule_id == schedule_id)
        .ok_or_else(|| anyhow!("schedule_id '{schedule_id}' has no recovery before-image"))?;
    let captured = image.signed_yaml.as_deref().map(str::as_bytes);
    let current = incumbent.map(|incumbent| incumbent.bytes.as_slice());
    if captured != current {
        anyhow::bail!(
            "schedule_id '{schedule_id}' source changed after its recovery before-image was captured"
        );
    }
    Ok(())
}

#[derive(Debug)]
struct ProjectManagedBy {
    project_key: String,
}

fn parse_project_managed_by(body: &Value) -> Option<ProjectManagedBy> {
    let managed_by = body.get("managed_by")?;
    let managed_type = managed_by.get("type")?.as_str()?;
    if managed_type != MANAGED_BY_TYPE {
        return None;
    }
    Some(ProjectManagedBy {
        project_key: managed_by.get("project_key")?.as_str()?.to_string(),
    })
}

fn load_project_managed_schedule_yaml(
    schedules_dir: &Path,
    project_key: &str,
    trust_store: &ryeos_engine::trust::TrustStore,
) -> Result<HashMap<String, Value>> {
    let mut out = HashMap::new();
    for path in ryeos_scheduler::projection::canonical_schedule_source_paths(schedules_dir)? {
        let schedule_id = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .expect("canonical schedule path has a validated UTF-8 stem");
        let verified =
            ryeos_scheduler::projection::load_verified_schedule_source(&path, trust_store)?;
        let body = serde_json::to_value(verified.record)?;
        let Some(managed) = parse_project_managed_by(&body) else {
            continue;
        };
        if managed.project_key == project_key && out.insert(schedule_id.to_string(), body).is_some()
        {
            anyhow::bail!(
                "multiple project-managed schedule sources declare schedule_id '{}'",
                schedule_id
            );
        }
    }
    Ok(out)
}

/// A current source read once through the pinned schedule directory: the
/// descriptor a later conditional write or removal must still find, and the
/// exact bytes that were verified. The bytes are kept because the descriptor's
/// read cursor is spent; a second read through it would observe nothing.
struct VerifiedIncumbent {
    file: lillux::PinnedRegularFile,
    bytes: Vec<u8>,
}

/// One no-follow read of the current source through the pinned schedule
/// directory, verified from exactly the bytes read.
fn read_pinned_verified_source(
    directory: &lillux::PinnedDirectory,
    schedule_id: &str,
    trust_store: &ryeos_engine::trust::TrustStore,
) -> Result<
    Option<(
        VerifiedIncumbent,
        ryeos_scheduler::projection::VerifiedScheduleSource,
    )>,
> {
    let name = format!("{schedule_id}.yaml");
    let path = directory.path().join(&name);
    let Some(file) = directory
        .open_pinned_regular(std::ffi::OsStr::new(&name), false)
        .with_context(|| format!("open schedule source {}", path.display()))?
    else {
        return Ok(None);
    };
    let bytes = file.read_bounded(MAX_RECOVERY_SCHEDULE_SOURCE_BYTES)?;
    let content = std::str::from_utf8(&bytes)
        .with_context(|| format!("schedule source {} is not UTF-8", path.display()))?;
    let verified =
        ryeos_scheduler::projection::verify_schedule_source_content(&path, content, trust_store)
            .with_context(|| format!("verify existing schedule YAML {}", path.display()))?;
    Ok(Some((VerifiedIncumbent { file, bytes }, verified)))
}

/// Re-prove one planned action under the held directory lock and return the
/// verified incumbent source (absent for a create).
fn revalidate_action(
    action: &ScheduleAction,
    ctx: &ProjectContext,
    directory: &lillux::PinnedDirectory,
) -> Result<Option<VerifiedIncumbent>> {
    let canonical_project_path = project_path_identity(ctx.request.project_path.as_path())?;
    match action {
        ScheduleAction::Create(desired) => {
            let schedule_id = &desired.declaration.schedule_id;
            if ctx.db.get_spec(schedule_id)?.is_some() {
                anyhow::bail!(
                    "schedule_id '{}' changed during project deploy; refusing to overwrite existing schedule",
                    schedule_id
                );
            }
            if read_pinned_verified_source(directory, schedule_id, &ctx.trust_store)?.is_some() {
                anyhow::bail!(
                    "schedule_id '{}' gained node YAML during project deploy; refusing to overwrite",
                    schedule_id
                );
            }
            reject_schedule_history_reuse(ctx, schedule_id)?;
            Ok(None)
        }
        ScheduleAction::Update {
            desired,
            existing,
            adopt_manual,
        } => {
            let schedule_id = &desired.declaration.schedule_id;
            let current = ctx.db.get_spec(schedule_id)?.ok_or_else(|| {
                anyhow!(
                    "schedule_id '{}' disappeared during project deploy; refusing update",
                    schedule_id
                )
            })?;
            if current.spec_hash != existing.spec_hash
                || current.registered_at != existing.registered_at
                || current.execution.principal_id() != existing.execution.principal_id()
            {
                anyhow::bail!(
                    "schedule_id '{}' changed during project deploy; retry project sync",
                    schedule_id
                );
            }
            let (incumbent, verified) = read_pinned_verified_source(
                directory,
                schedule_id,
                &ctx.trust_store,
            )?
            .ok_or_else(|| {
                anyhow!(
                    "schedule_id '{}' lost node YAML during project deploy; refusing update",
                    schedule_id
                )
            })?;
            if verified.spec_hash != existing.spec_hash {
                anyhow::bail!(
                    "schedule_id '{}' YAML changed during project deploy; retry project sync",
                    schedule_id
                );
            }
            let body = serde_json::to_value(&verified.record)?;
            match parse_project_managed_by(&body) {
                Some(managed) => {
                    if managed.project_key != ctx.request.project_key.as_str() {
                        anyhow::bail!(
                            "schedule_id '{}' changed project ownership during project deploy",
                            schedule_id
                        );
                    }
                }
                None if *adopt_manual => {
                    if current.project_root.as_deref() != Some(canonical_project_path) {
                        anyhow::bail!(
                            "schedule_id '{}' changed project_root during project deploy; refusing manual adoption",
                            schedule_id
                        );
                    }
                    require_project_reconcile_schedule_owner(
                        ctx.request.caller.as_ref(),
                        schedule_id,
                        current.execution.principal_id(),
                    )?;
                }
                None => {
                    anyhow::bail!(
                        "schedule_id '{}' is no longer project-managed during project deploy",
                        schedule_id
                    );
                }
            }
            Ok(Some(incumbent))
        }
        ScheduleAction::DeleteMissing {
            schedule_id,
            existing,
        } => {
            let current = ctx.db.get_spec(schedule_id)?.ok_or_else(|| {
                anyhow!(
                    "schedule_id '{}' disappeared during project deploy; refusing delete",
                    schedule_id
                )
            })?;
            if current.spec_hash != existing.spec_hash
                || current.registered_at != existing.registered_at
                || current.execution.principal_id() != existing.execution.principal_id()
            {
                anyhow::bail!(
                    "schedule_id '{}' changed during project deploy; refusing delete",
                    schedule_id
                );
            }
            let (incumbent, verified) = read_pinned_verified_source(
                directory,
                schedule_id,
                &ctx.trust_store,
            )?
            .ok_or_else(|| {
                anyhow!(
                    "schedule_id '{}' lost node YAML during project deploy; refusing delete",
                    schedule_id
                )
            })?;
            if verified.spec_hash != existing.spec_hash {
                anyhow::bail!(
                    "schedule_id '{}' YAML changed during project deploy; refusing delete",
                    schedule_id
                );
            }
            let body = serde_json::to_value(&verified.record)?;
            let managed = parse_project_managed_by(&body).ok_or_else(|| {
                anyhow!(
                    "schedule_id '{}' is no longer project-managed during project deploy",
                    schedule_id
                )
            })?;
            if managed.project_key != ctx.request.project_key.as_str() {
                anyhow::bail!(
                    "schedule_id '{}' changed project ownership during project deploy",
                    schedule_id
                );
            }
            // Same-project is now re-proven (managed_by.project_key matches);
            // only here do we surface an ownership conflict (409), never before
            // the project-scope check.
            require_project_reconcile_schedule_owner(
                ctx.request.caller.as_ref(),
                schedule_id,
                current.execution.principal_id(),
            )?;
            Ok(Some(incumbent))
        }
    }
}

pub fn validate_project_declarations(staging_root: &Path, project_path: &Path) -> Result<usize> {
    Ok(load_desired_schedules(staging_root, project_path)?.len())
}

fn load_desired_schedules(
    staging_root: &Path,
    project_path: &Path,
) -> Result<Vec<DesiredSchedule>> {
    let schedules_root = staging_root.join(".ai/config/schedules");
    if !schedules_root.is_dir() {
        return Ok(Vec::new());
    }

    let canonical_project_path = project_path_identity(project_path)?;
    let mut seen = HashSet::new();
    let mut desired = Vec::new();
    for path in schedule_declaration_files(&schedules_root)? {
        let relative = path.strip_prefix(staging_root).unwrap_or(&path);
        let rel_path = relative
            .to_str()
            .ok_or_else(|| {
                anyhow!(
                    "schedule declaration path '{}' is not valid UTF-8",
                    relative.display()
                )
            })?
            .replace('\\', "/");
        let content = fs::read_to_string(&path)
            .with_context(|| format!("read schedule declaration {}", rel_path))?;
        let body = lillux::signature::strip_signature_lines(&content);
        let file: ScheduleDeclarationFile = serde_yaml::from_str(&body)
            .with_context(|| format!("parse schedule declaration {}", rel_path))?;
        validate_schedule_declaration_file(&file, &rel_path)?;
        let source_body_hash = lillux::cas::sha256_hex(body.as_bytes());
        for schedule in file.schedules {
            validate_schedule_declaration(&schedule, &rel_path, canonical_project_path)?;
            if !seen.insert(schedule.schedule_id.clone()) {
                anyhow::bail!(
                    "duplicate schedule_id '{}' across project schedule declarations",
                    schedule.schedule_id
                );
            }
            desired.push(DesiredSchedule {
                declaration: schedule,
                source_path: rel_path.clone(),
                source_body_hash: source_body_hash.clone(),
            });
        }
    }
    Ok(desired)
}

fn schedule_declaration_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    collect_schedule_declaration_files(root, &mut files)?;
    files.sort();
    Ok(files)
}

fn collect_schedule_declaration_files(dir: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        let ft = entry.file_type()?;
        if ft.is_symlink() {
            anyhow::bail!(
                "schedule declaration path '{}' is a symlink; refusing project deploy",
                path.display()
            );
        }
        if ft.is_dir() {
            collect_schedule_declaration_files(&path, files)?;
        } else if ft.is_file() {
            let ext = path.extension().and_then(|ext| ext.to_str());
            if ext == Some("yaml") {
                files.push(path);
            } else {
                anyhow::bail!(
                    "schedule declaration path '{}' is not canonical YAML; expected .yaml",
                    path.display()
                );
            }
        }
    }
    Ok(())
}

fn validate_schedule_declaration_file(
    file: &ScheduleDeclarationFile,
    rel_path: &str,
) -> Result<()> {
    if file.category != "schedules" {
        anyhow::bail!(
            "schedule declaration '{}' has category '{}', expected 'schedules'",
            rel_path,
            file.category
        );
    }
    if file.version != "1.0.0" {
        anyhow::bail!(
            "schedule declaration '{}' has unsupported version '{}', expected '1.0.0'",
            rel_path,
            file.version
        );
    }
    if file.schema_version != "1.0.0" {
        anyhow::bail!(
            "schedule declaration '{}' has unsupported schema_version '{}', expected '1.0.0'",
            rel_path,
            file.schema_version
        );
    }
    if file.schedules.is_empty() {
        anyhow::bail!("schedule declaration '{}' contains no schedules", rel_path);
    }
    Ok(())
}

fn validate_schedule_declaration(
    schedule: &ScheduleDeclaration,
    rel_path: &str,
    canonical_project_path: &str,
) -> Result<()> {
    ryeos_scheduler_contract::validate_schedule_id(&schedule.schedule_id)
        .with_context(|| format!("invalid schedule_id in {}", rel_path))?;
    ryeos_engine::canonical_ref::CanonicalRef::parse(&schedule.item_ref)
        .with_context(|| format!("invalid item_ref for schedule '{}'", schedule.schedule_id))?;
    ryeos_executor::execution::launch_preparation::validate_ref_bindings(&schedule.ref_bindings)
        .with_context(|| {
            format!(
                "invalid ref_bindings for schedule '{}'",
                schedule.schedule_id
            )
        })?;
    ryeos_scheduler_contract::validate_expression(&schedule.schedule_type, &schedule.expression)
        .with_context(|| format!("invalid expression for schedule '{}'", schedule.schedule_id))?;
    if schedule.schedule_type == "at"
        && ryeos_scheduler::crontab::is_at_past(
            &schedule.expression,
            lillux::time::timestamp_millis(),
        )
    {
        anyhow::bail!(
            "at schedule timestamp is in the past for schedule '{}'",
            schedule.schedule_id
        );
    }
    ryeos_scheduler_contract::validate_timezone(&schedule.timezone)
        .with_context(|| format!("invalid timezone for schedule '{}'", schedule.schedule_id))?;
    ryeos_scheduler_contract::parse_overlap_policy(&schedule.overlap_policy).with_context(
        || {
            format!(
                "invalid overlap_policy for schedule '{}'",
                schedule.schedule_id
            )
        },
    )?;
    ryeos_scheduler_contract::parse_misfire_policy(&schedule.misfire_policy).with_context(
        || {
            format!(
                "invalid misfire_policy for schedule '{}'",
                schedule.schedule_id
            )
        },
    )?;
    if schedule.lateness_grace_secs <= 0 {
        anyhow::bail!(
            "lateness_grace_secs must be positive for schedule '{}'",
            schedule.schedule_id
        );
    }
    if !schedule.params.is_object() {
        anyhow::bail!(
            "params must be a mapping for schedule '{}'",
            schedule.schedule_id
        );
    }
    if schedule.capabilities.is_empty()
        || schedule
            .capabilities
            .iter()
            .any(|capability| capability.trim().is_empty())
        || schedule
            .capabilities
            .windows(2)
            .any(|pair| pair[0] >= pair[1])
    {
        anyhow::bail!(
            "capabilities must be non-empty, sorted, and unique for schedule '{}'",
            schedule.schedule_id
        );
    }
    schedule.execution_policy.validate().with_context(|| {
        format!(
            "invalid execution_policy for schedule '{}'",
            schedule.schedule_id
        )
    })?;
    if matches!(
        &schedule.execution_policy.project,
        ryeos_engine::execution_contract::ProjectExecutionPolicy::Projectless
    ) {
        anyhow::bail!(
            "project-managed schedule '{}' must use a project-backed execution policy; register projectless work directly with the scheduler service",
            schedule.schedule_id
        );
    }
    if let Some(ref project_root) = schedule.project_root {
        let declared = Path::new(project_root);
        if !declared.is_absolute() {
            anyhow::bail!(
                "project_root for schedule '{}' must be absolute",
                schedule.schedule_id
            );
        }
        if project_root != canonical_project_path {
            anyhow::bail!(
                "project_root for schedule '{}' is '{}', expected '{}'; project schedule declarations cannot target another project",
                schedule.schedule_id,
                project_root,
                canonical_project_path
            );
        }
    }
    Ok(())
}

fn require_schedule_registration_authority(ctx: &ProjectContext) -> Result<()> {
    ctx.request.caller.require_registration()
}

/// Ownership gate for deploy-reconcile schedule actions (Update / DeleteMissing).
///
/// Unlike `HandlerContext::require_owner` (which returns `NotFound` to hide
/// existence on *direct* resource access), this returns a descriptive
/// `Conflict` (409). Inside a project sync the caller is reconciling their own
/// snapshot, which *declares* this schedule — they already know it exists, so
/// hiding it behind a bare `404 {"error":"not found"}` only produced a failure
/// indistinguishable from a routing 404. The conflicting owner's fingerprint is
/// never disclosed. Still fails closed as `NotFound` for an unverified caller
/// (apply-snapshot requires a verified caller; this is defence in depth).
fn require_project_reconcile_schedule_owner(
    caller: &dyn ProjectScheduleCaller,
    schedule_id: &str,
    owner_fingerprint: &str,
) -> Result<()> {
    caller.require_owner(schedule_id, owner_fingerprint)
}

fn reject_schedule_history_reuse(ctx: &ProjectContext, schedule_id: &str) -> Result<()> {
    let fires_dir = ctx
        .app_root
        .join(ryeos_engine::AI_DIR)
        .join("state")
        .join("schedules")
        .join(schedule_id);
    match fs::symlink_metadata(&fires_dir) {
        Ok(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {
            anyhow::bail!(
                "schedule_id '{}' reuse not allowed: fire history exists at {} — deregister first or use a different ID",
                schedule_id,
                fires_dir.display()
            );
        }
        Ok(_) => anyhow::bail!(
            "schedule history path must be a real directory: {}",
            fires_dir.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn canonical_project_schedule_path(schedules_dir: &Path, schedule_id: &str) -> Result<PathBuf> {
    let _ = ryeos_scheduler::projection::canonical_schedule_source_paths(schedules_dir)?;
    Ok(schedules_dir.join(format!("{schedule_id}.yaml")))
}

fn read_existing_schedule_body(
    schedules_dir: &Path,
    schedule_id: &str,
    trust_store: &ryeos_engine::trust::TrustStore,
) -> Result<Option<Value>> {
    let yaml_path = canonical_project_schedule_path(schedules_dir, schedule_id)?;
    match fs::symlink_metadata(&yaml_path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("inspect schedule source {}", yaml_path.display()));
        }
    }
    let verified =
        ryeos_scheduler::projection::load_verified_schedule_source(&yaml_path, trust_store)
            .with_context(|| format!("verify existing schedule YAML {}", yaml_path.display()))?;
    Ok(Some(serde_json::to_value(verified.record)?))
}

fn write_reconciled_schedule(
    schedules_dir: &lillux::PinnedDirectory,
    expected: Option<&lillux::PinnedRegularFile>,
    desired: &DesiredSchedule,
    ctx: &ProjectContext,
    registered_at: i64,
) -> Result<()> {
    if desired.declaration.capabilities.is_empty() {
        anyhow::bail!(
            "project schedule '{}' cannot be reconciled without execution requester and capabilities",
            desired.declaration.schedule_id
        );
    }

    let schedule = &desired.declaration;
    let canonical_project_path =
        project_path_identity(ctx.request.project_path.as_path())?.to_owned();
    let capabilities = desired.declaration.capabilities.clone();
    for capability in &capabilities {
        ctx.request.caller.require_capability(capability).map_err(|error| {
            anyhow!(
                "project schedule '{}' capability {:?} is not covered by the authenticated deployment grant: {error:#}",
                schedule.schedule_id,
                capability,
            )
        })?;
    }
    let registration_request_hash =
        ryeos_state::objects::canonical_value_digest(&serde_json::json!({
            "operation": "project_schedule_registration",
            "project_key": ctx.request.project_key.as_str(),
            "project_snapshot_hash": ctx.request.snapshot_hash.as_str(),
            "source_path": desired.source_path,
            "source_body_hash": desired.source_body_hash,
            "schedule": schedule,
        }))?;
    let authority = ctx
        .request
        .caller
        .authenticated_execution_authority(registration_request_hash)?;
    let body = serde_json::json!({
        "spec_version": 2,
        "schedule_id": schedule.schedule_id,
        "item_ref": schedule.item_ref,
        "ref_bindings": schedule.ref_bindings,
        "schedule_type": schedule.schedule_type,
        "expression": schedule.expression,
        "timezone": schedule.timezone,
        "enabled": schedule.enabled,
        "registered_at": registered_at,
        "misfire_policy": schedule.misfire_policy,
        "overlap_policy": schedule.overlap_policy,
        "lateness_grace_secs": schedule.lateness_grace_secs,
        "params": schedule.params,
        "project_root": canonical_project_path,
        "execution": {
            "authority": authority,
            "capabilities": capabilities,
            "policy": schedule.execution_policy,
        },
        "managed_by": {
            "type": MANAGED_BY_TYPE,
            "project_root": canonical_project_path,
            "project_key": ctx.request.project_key.as_str(),
            "source_snapshot_hash": ctx.request.snapshot_hash.as_str(),
            "source_path": desired.source_path,
            "source_body_hash": desired.source_body_hash,
        },
    });

    let bytes = node_document::render_signed_item(
        "schedules",
        &schedule.schedule_id,
        &body,
        &ctx.identity,
    )?;
    let filename = format!("{}.yaml", schedule.schedule_id);
    let filename = std::ffi::OsStr::new(&filename);
    schedules_dir.atomic_write_pinned_if_same(
        filename,
        expected,
        &bytes,
        0o600,
        crate::SCHEDULER_TEMP_PURPOSE,
    )?;
    let spec_path = schedules_dir.path().join(filename);
    let content = String::from_utf8(bytes).context("signed schedule source is not UTF-8")?;
    let verified = ryeos_scheduler::projection::verify_schedule_source_content(
        &spec_path,
        &content,
        &ctx.trust_store,
    )?;
    let rec = verified.to_spec_record()?;
    ctx.db.upsert_spec(&rec)?;
    Ok(())
}

#[derive(Debug)]
struct ScheduleBackup {
    schedule_id: String,
    yaml_bytes: Option<Vec<u8>>,
    db_record: Option<ScheduleSpecRecord>,
}

#[derive(Debug)]
struct ScheduleReconcileTx {
    directory: lillux::PinnedDirectory,
    _directory_lock: lillux::PinnedDirectoryLock,
    backups: Vec<ScheduleBackup>,
    touched: HashSet<String>,
}

impl ScheduleReconcileTx {
    fn new(schedules_dir: &Path) -> Result<Self> {
        let directory = lillux::PinnedDirectory::open_or_create(schedules_dir)?;
        let directory_lock = directory.lock_exclusive()?;
        Ok(Self {
            directory,
            _directory_lock: directory_lock,
            backups: Vec::new(),
            touched: HashSet::new(),
        })
    }

    fn new_with_timeout(schedules_dir: &Path, timeout: lillux::time::Duration) -> Result<Self> {
        let directory = lillux::PinnedDirectory::open_or_create(schedules_dir)?;
        let directory_lock = directory.lock_exclusive_with_timeout(timeout)?;
        Ok(Self {
            directory,
            _directory_lock: directory_lock,
            backups: Vec::new(),
            touched: HashSet::new(),
        })
    }

    fn schedule_path(&self, schedule_id: &str) -> PathBuf {
        self.directory.path().join(format!("{schedule_id}.yaml"))
    }

    fn backup(
        &mut self,
        ctx: &ProjectContext,
        schedule_id: &str,
        incumbent: Option<&VerifiedIncumbent>,
    ) -> Result<()> {
        if self.backups.iter().any(|b| b.schedule_id == schedule_id) {
            return Ok(());
        }
        let yaml_bytes = incumbent.map(|incumbent| incumbent.bytes.clone());
        let db_record = ctx.db.get_spec(schedule_id)?;
        self.backups.push(ScheduleBackup {
            schedule_id: schedule_id.to_string(),
            yaml_bytes,
            db_record,
        });
        Ok(())
    }

    fn touch(&mut self, schedule_id: String) {
        self.touched.insert(schedule_id);
    }

    fn finalize(&self, ctx: &ProjectContext) {
        reload_touched(ctx, &self.touched);
    }

    fn rollback(&mut self, ctx: &ProjectContext) -> Result<()> {
        for backup in self.backups.iter().rev() {
            let yaml_path = self.schedule_path(&backup.schedule_id);
            let name = yaml_path
                .file_name()
                .ok_or_else(|| anyhow!("schedule path has no filename"))?;
            let expected = self.directory.open_pinned_regular(name, false)?;
            match &backup.yaml_bytes {
                Some(bytes) => {
                    self.directory.atomic_write_pinned_if_same(
                        name,
                        expected.as_ref(),
                        bytes,
                        0o600,
                        crate::SCHEDULER_TEMP_PURPOSE,
                    )?;
                }
                None => {
                    if let Some(file) = expected {
                        self.directory.remove_pinned_regular_if_same(&file)?;
                    }
                }
            }

            match &backup.db_record {
                Some(record) => {
                    ctx.db.upsert_spec(record)?;
                }
                None => {
                    ctx.db.delete_spec(&backup.schedule_id)?;
                }
            }
            self.touched.insert(backup.schedule_id.clone());
        }
        reload_touched(ctx, &self.touched);
        Ok(())
    }
}

fn reload_touched(ctx: &ProjectContext, touched: &HashSet<String>) {
    if let Some(tx) = ctx.reload_tx.as_ref() {
        for schedule_id in touched {
            if let Err(e) = tx.try_send(ryeos_scheduler::ReloadSignal {
                schedule_id: Some(schedule_id.clone()),
            }) {
                tracing::warn!(schedule_id = %schedule_id, error = %e, "scheduler reload channel full or closed — timer will pick up changes on next tick");
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SessionPhase {
    Acquired,
    Planned,
    RecoveryPrepared,
    Prepared,
    Finalized,
    RolledBack,
    RecoveryRequired,
    Restored,
}

struct ProjectScheduleSessionImpl {
    _runtime_gate: OwnedRwLockWriteGuard<()>,
    app_root: PathBuf,
    db: Arc<ScheduleDb>,
    identity: Arc<ryeos_node_runtime::identity::NodeIdentity>,
    trust_store: Arc<ryeos_engine::trust::TrustStore>,
    reload_tx: Option<tokio::sync::mpsc::Sender<ryeos_scheduler::ReloadSignal>>,
    recovery_pending: Arc<std::sync::atomic::AtomicBool>,
    request: Option<Arc<ProjectScheduleDeployRequest>>,
    plan: Option<ScheduleDeployPlan>,
    recovery: Option<ScheduleRecoveryPreparation>,
    phase: SessionPhase,
}

impl ProjectScheduleSessionImpl {
    fn new(service: &SchedulerService, runtime_gate: OwnedRwLockWriteGuard<()>) -> Self {
        let reload_tx = service.reload_tx.read().ok().and_then(|slot| slot.clone());
        Self {
            _runtime_gate: runtime_gate,
            app_root: service.app_root.clone(),
            db: service.db.clone(),
            identity: service.identity.clone(),
            trust_store: service.trust_store.clone(),
            reload_tx,
            recovery_pending: service.project_recovery_pending.clone(),
            request: None,
            plan: None,
            recovery: None,
            phase: SessionPhase::Acquired,
        }
    }

    fn context(&self) -> Result<ProjectContext> {
        let request = self
            .request
            .as_ref()
            .ok_or_else(|| anyhow!("project schedule session has no plan request"))?;
        Ok(ProjectContext {
            request: Arc::clone(request),
            app_root: self.app_root.clone(),
            db: self.db.clone(),
            trust_store: self.trust_store.clone(),
            identity: self.identity.clone(),
            reload_tx: self.reload_tx.clone(),
        })
    }
}

impl ProjectScheduleDeployParticipant for SchedulerService {
    fn acquire_project_schedules(
        &self,
    ) -> ProjectScheduleFuture<'_, Box<dyn ProjectScheduleSession>> {
        Box::pin(async move {
            let runtime_gate = self.runtime_gate.clone().write_owned().await;
            Ok(
                Box::new(ProjectScheduleSessionImpl::new(self, runtime_gate))
                    as Box<dyn ProjectScheduleSession>,
            )
        })
    }

    fn try_acquire_project_schedules(&self) -> Result<Option<Box<dyn ProjectScheduleSession>>> {
        let Some(runtime_gate) = try_project_schedule_write_gate(self.runtime_gate.clone()) else {
            return Ok(None);
        };
        if self
            .project_recovery_pending
            .load(std::sync::atomic::Ordering::Acquire)
        {
            anyhow::bail!("project schedule mutation is blocked by unresolved recovery");
        }
        Ok(Some(Box::new(ProjectScheduleSessionImpl::new(
            self,
            runtime_gate,
        ))))
    }
}

fn try_project_schedule_write_gate(
    runtime_gate: Arc<RwLock<()>>,
) -> Option<OwnedRwLockWriteGuard<()>> {
    runtime_gate.try_write_owned().ok()
}

#[cfg(test)]
mod project_gate_tests {
    use super::*;

    #[tokio::test]
    async fn pending_recovery_gate_probe_does_not_wait_for_an_existing_owner() {
        let runtime_gate = Arc::new(RwLock::new(()));
        let retained_owner = runtime_gate.clone().write_owned().await;

        assert!(try_project_schedule_write_gate(Arc::clone(&runtime_gate)).is_none());
        drop(retained_owner);
        assert!(try_project_schedule_write_gate(runtime_gate).is_some());
    }
}

impl ProjectScheduleSession for ProjectScheduleSessionImpl {
    fn plan(&mut self, request: ProjectScheduleDeployRequest) -> Result<ScheduleDeployReport> {
        if self.phase != SessionPhase::Acquired {
            anyhow::bail!("project schedule session cannot plan in its current phase");
        }
        if self
            .recovery_pending
            .load(std::sync::atomic::Ordering::Acquire)
        {
            anyhow::bail!("project schedule mutation is blocked by unresolved recovery");
        }
        self.request = Some(Arc::new(request));
        let context = self.context()?;
        let plan = plan(&context)?;
        let report = ScheduleDeployReport {
            declared: plan.declared,
            ..ScheduleDeployReport::default()
        };
        self.plan = Some(plan);
        self.phase = SessionPhase::Planned;
        Ok(report)
    }

    fn has_actions(&self) -> Result<bool> {
        if self.phase != SessionPhase::Planned {
            anyhow::bail!("project schedule session has not completed planning");
        }
        Ok(self
            .plan
            .as_ref()
            .is_some_and(|plan| !plan.actions.is_empty()))
    }

    fn prepare_recovery(&mut self) -> ProjectScheduleFuture<'_, ()> {
        Box::pin(async move {
            if self.phase != SessionPhase::Planned {
                anyhow::bail!(
                    "project schedule session cannot prepare recovery in its current phase"
                );
            }
            let plan = self
                .plan
                .as_ref()
                .ok_or_else(|| anyhow!("project schedule session has no plan"))?;
            let context = self.context()?;
            self.recovery = Some(prepare_recovery_before_images(plan, &context).await?);
            self.phase = SessionPhase::RecoveryPrepared;
            Ok(())
        })
    }

    fn before_images(&self) -> Result<&[ScheduleRecoveryBeforeImage]> {
        self.recovery
            .as_ref()
            .map(ScheduleRecoveryPreparation::before_images)
            .ok_or_else(|| anyhow!("project schedule recovery images are not prepared"))
    }

    fn prepare(&mut self) -> Result<ScheduleDeployReport> {
        if self.phase != SessionPhase::RecoveryPrepared {
            anyhow::bail!("project schedule session cannot prepare in its current phase");
        }
        let plan = self
            .plan
            .as_ref()
            .ok_or_else(|| anyhow!("project schedule session has no plan"))?;
        let context = self.context()?;
        let recovery = self
            .recovery
            .as_mut()
            .ok_or_else(|| anyhow!("project schedule recovery lock was lost"))?;
        match prepare_commit_with_recovery(plan, &context, recovery) {
            Ok(report) => {
                self.phase = SessionPhase::Prepared;
                Ok(report)
            }
            Err(error) => {
                self.phase = SessionPhase::RecoveryRequired;
                self.recovery_pending
                    .store(true, std::sync::atomic::Ordering::Release);
                Err(error)
            }
        }
    }

    fn finalize(&mut self) -> Result<()> {
        if self.phase == SessionPhase::Finalized {
            return Ok(());
        }
        if self.phase != SessionPhase::Prepared
            && !matches!(
                self.phase,
                SessionPhase::RecoveryPrepared | SessionPhase::RecoveryRequired
            ) {
                anyhow::bail!("project schedule session cannot finalize in its current phase");
            }
        let recovery = self
            .recovery
            .as_ref()
            .ok_or_else(|| anyhow!("project schedule recovery lock was lost"))?;
        if self.phase == SessionPhase::Prepared {
            recovery.tx.finalize(&self.context()?);
        } else {
            let touched = recovery
                .before_images
                .iter()
                .map(|entry| entry.schedule_id.clone())
                .collect::<HashSet<_>>();
            reload_touched_sender(self.reload_tx.as_ref(), &touched);
        }
        self.phase = SessionPhase::Finalized;
        self.recovery_pending
            .store(false, std::sync::atomic::Ordering::Release);
        Ok(())
    }

    fn rollback(&mut self) -> Result<()> {
        if matches!(
            self.phase,
            SessionPhase::RolledBack | SessionPhase::Restored
        ) {
            return Ok(());
        }
        // A finalized deploy is committed: its retained backups must never
        // revert it. Only a captured, not yet resolved session may roll back.
        if !matches!(
            self.phase,
            SessionPhase::RecoveryPrepared
                | SessionPhase::Prepared
                | SessionPhase::RecoveryRequired
        ) {
            anyhow::bail!("project schedule session cannot rollback in its current phase");
        }
        let context = self.context()?;
        let recovery = self
            .recovery
            .as_mut()
            .ok_or_else(|| anyhow!("project schedule recovery lock was lost"))?;
        if let Err(error) = recovery.tx.rollback(&context) {
            self.phase = SessionPhase::RecoveryRequired;
            self.recovery_pending
                .store(true, std::sync::atomic::Ordering::Release);
            return Err(error).context("restore project schedule before-images");
        }
        self.phase = SessionPhase::RolledBack;
        self.recovery_pending
            .store(false, std::sync::atomic::Ordering::Release);
        Ok(())
    }

    fn retain_for_recovery(&mut self) -> Result<()> {
        if !matches!(
            self.phase,
            SessionPhase::RecoveryPrepared
                | SessionPhase::Prepared
                | SessionPhase::RecoveryRequired
        ) {
            anyhow::bail!("project schedule session cannot retain recovery in its current phase");
        }
        self.phase = SessionPhase::RecoveryRequired;
        self.recovery_pending
            .store(true, std::sync::atomic::Ordering::Release);
        Ok(())
    }

    fn prepare_restoration<'a>(
        &'a mut self,
        entries: &'a [ScheduleRecoveryBeforeImage],
    ) -> ProjectScheduleFuture<'a, ()> {
        Box::pin(async move {
            if let Some(preparation) = &self.recovery {
                if preparation.before_images.len() != entries.len()
                    || preparation
                        .before_images
                        .iter()
                        .zip(entries)
                        .any(|(left, right)| {
                            left.schedule_id != right.schedule_id
                                || left.signed_yaml != right.signed_yaml
                        })
                {
                    anyhow::bail!(
                        "journal schedule images disagree with the retained project session"
                    );
                }
                match self.phase {
                    SessionPhase::RecoveryPrepared
                    | SessionPhase::Finalized
                    | SessionPhase::Restored
                    | SessionPhase::RolledBack => {}
                    SessionPhase::Prepared | SessionPhase::RecoveryRequired => {
                        self.phase = SessionPhase::RecoveryRequired;
                    }
                    _ => anyhow::bail!(
                        "project schedule session cannot reuse recovery preparation in its current phase"
                    ),
                }
                return Ok(());
            }
            if !matches!(
                self.phase,
                SessionPhase::Acquired | SessionPhase::RecoveryRequired
            ) {
                anyhow::bail!(
                    "project schedule session cannot prepare restoration in its current phase"
                );
            }
            let app_root = self.app_root.clone();
            let entries = entries.to_vec();
            let preparation =
                tokio::task::spawn_blocking(move || -> Result<ScheduleRecoveryPreparation> {
                    let schedules_dir = app_root.join(ryeos_engine::AI_DIR).join("node/schedules");
                    let tx = ScheduleReconcileTx::new_with_timeout(
                        &schedules_dir,
                        SCHEDULE_DIRECTORY_LOCK_TIMEOUT,
                    )?;
                    let mut previous: Option<&str> = None;
                    for entry in &entries {
                        ryeos_scheduler_contract::validate_schedule_id(&entry.schedule_id)?;
                        if entry.signed_yaml.as_ref().is_some_and(|yaml| {
                            yaml.len() as u64 > MAX_RECOVERY_SCHEDULE_SOURCE_BYTES
                        }) {
                            anyhow::bail!("schedule recovery source exceeds its byte limit");
                        }
                        if previous.is_some_and(|value| value >= entry.schedule_id.as_str()) {
                            anyhow::bail!(
                                "journal schedule before-images must be sorted and unique"
                            );
                        }
                        previous = Some(&entry.schedule_id);
                    }
                    Ok(ScheduleRecoveryPreparation {
                        before_images: entries,
                        tx,
                    })
                })
                .await
                .context("join project schedule restoration preparation")??;
            self.recovery = Some(preparation);
            self.phase = SessionPhase::RecoveryPrepared;
            Ok(())
        })
    }

    fn restore(&mut self) -> Result<()> {
        if matches!(
            self.phase,
            SessionPhase::Restored | SessionPhase::RolledBack
        ) {
            return Ok(());
        }
        if !matches!(
            self.phase,
            SessionPhase::RecoveryPrepared | SessionPhase::RecoveryRequired
        ) {
            anyhow::bail!("project schedule session cannot restore in its current phase");
        }
        let recovery = self
            .recovery
            .as_ref()
            .ok_or_else(|| anyhow!("project schedule restoration lock was lost"))?;
        let entries = recovery.before_images.clone();
        let touched = match restore_recovery_sources_and_projection(
            &recovery.tx.directory,
            &self.db,
            &self.trust_store,
            &entries,
        ) {
            Ok(touched) => touched,
            Err(error) => {
                self.phase = SessionPhase::RecoveryRequired;
                self.recovery_pending
                    .store(true, std::sync::atomic::Ordering::Release);
                return Err(error)
                    .context("restore authenticated project schedule sources and projection");
            }
        };
        reload_touched_sender(self.reload_tx.as_ref(), &touched);
        self.phase = SessionPhase::Restored;
        self.recovery_pending
            .store(false, std::sync::atomic::Ordering::Release);
        Ok(())
    }
}

impl Drop for ProjectScheduleSessionImpl {
    fn drop(&mut self) {
        if matches!(
            self.phase,
            SessionPhase::Prepared | SessionPhase::RecoveryRequired
        ) {
            self.recovery_pending
                .store(true, std::sync::atomic::Ordering::Release);
        }
    }
}

fn reload_touched_sender(
    sender: Option<&tokio::sync::mpsc::Sender<ryeos_scheduler::ReloadSignal>>,
    touched: &HashSet<String>,
) {
    if let Some(sender) = sender {
        for schedule_id in touched {
            if let Err(error) = sender.try_send(ryeos_scheduler::ReloadSignal {
                schedule_id: Some(schedule_id.clone()),
            }) {
                tracing::warn!(%schedule_id, %error, "scheduler reload notification failed; timer repair will reconcile changes");
            }
        }
    }
}

/// Project deployment evidence against a real scheduler store: exact signed
/// before-images (absence included), rollback of every action kind, restore
/// from journal-shaped images after the store is reopened, and the recovery
/// owner kept when a session is dropped or its rollback fails.
#[cfg(test)]
mod deploy_recovery_tests {
    use super::*;
    use ryeos_scheduler::test_native::TestSchedulerHost;
    use ryeos_scheduler_contract::ScheduleExecutionAuthority;
    use std::sync::atomic::Ordering;

    const ALPHA: &str = "project-alpha";
    const BRAVO: &str = "project-bravo";
    const CHARLIE: &str = "project-charlie";
    const OBSERVED: [&str; 3] = [ALPHA, BRAVO, CHARLIE];

    /// A caller holding every deployment grant; it signs schedules under the
    /// node's own execution authority so the fixture needs no authorized key.
    struct GrantedCaller {
        identity: Arc<ryeos_node_runtime::identity::NodeIdentity>,
    }

    impl ProjectScheduleCaller for GrantedCaller {
        fn require_registration(&self) -> Result<()> {
            Ok(())
        }

        fn require_owner(&self, _: &str, _: &str) -> Result<()> {
            Ok(())
        }

        fn require_capability(&self, _: &str) -> Result<()> {
            Ok(())
        }

        fn authenticated_execution_authority(
            &self,
            _: String,
        ) -> Result<ScheduleExecutionAuthority> {
            Ok(ScheduleExecutionAuthority::Node {
                principal_id: self.identity.principal_id(),
                effective_origin_site_id: self.identity.site_id(),
            })
        }
    }

    struct Fixture {
        root: tempfile::TempDir,
        app_root: PathBuf,
        runtime: PathBuf,
        project: PathBuf,
        identity: Arc<ryeos_node_runtime::identity::NodeIdentity>,
        trust: Arc<ryeos_engine::trust::TrustStore>,
    }

    /// One open scheduler store and the service bound to it.
    struct Node {
        db: Arc<ScheduleDb>,
        service: Arc<SchedulerService>,
    }

    type Observation = BTreeMap<String, (Option<Vec<u8>>, Option<Value>)>;

    impl Fixture {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let app_root = root.path().join("app");
            let runtime = app_root.join(ryeos_engine::AI_DIR).join("state");
            fs::create_dir_all(&runtime).unwrap();
            fs::create_dir_all(app_root.join(ryeos_engine::AI_DIR).join("node/schedules")).unwrap();
            let project = root.path().join("project");
            fs::create_dir_all(&project).unwrap();
            let project = project.canonicalize().unwrap();
            let identity = Arc::new(
                ryeos_node_runtime::identity::NodeIdentity::create(
                    &root.path().join("identity.key"),
                )
                .unwrap(),
            );
            let trust = Arc::new(ryeos_engine::trust::TrustStore::from_signers(vec![
                ryeos_engine::trust::TrustedSigner {
                    fingerprint: identity.fingerprint().to_owned(),
                    verifying_key: *identity.verifying_key(),
                    label: Some("project-deploy-test-node".to_owned()),
                },
            ]));
            Self {
                root,
                app_root,
                runtime,
                project,
                identity,
                trust,
            }
        }

        /// Create the store as a node's first start does: its fresh fire
        /// projection is published once.
        fn open(&self, host: &TestSchedulerHost) -> Node {
            self.open_store(host, true)
        }

        /// Reopen the existing store as a restarted node does.
        fn reopen(&self, host: &TestSchedulerHost) -> Node {
            self.open_store(host, false)
        }

        fn open_store(&self, host: &TestSchedulerHost, fresh: bool) -> Node {
            let db = match host.open_at(&self.runtime) {
                Ok(db) => Arc::new(db),
                Err(failure) => {
                    let error = failure.to_string();
                    host.settle_refusal(failure);
                    panic!("open test scheduler store: {error}");
                }
            };
            if fresh {
                db.finish_fire_projection_rebuild().unwrap();
            }
            let runtime_directory = Arc::new(
                lillux::PinnedDirectory::open(&self.runtime)
                    .unwrap()
                    .unwrap(),
            );
            let service = Arc::new(
                SchedulerService::new(
                    self.app_root.clone(),
                    runtime_directory,
                    Arc::clone(&db),
                    Arc::new(RwLock::new(())),
                    Arc::clone(&self.identity),
                    Arc::clone(&self.trust),
                )
                .unwrap(),
            );
            Node { db, service }
        }

        /// Close the service and its store the way a stopping node does: the
        /// drained gate closes the store, then the last alias settles it.
        fn close(&self, host: &TestSchedulerHost, node: Node) {
            let Node { db, service } = node;
            service.close_after_shutdown_drain().unwrap();
            drop(service);
            host.settle_shared(db);
        }

        fn schedules_dir(&self) -> PathBuf {
            self.app_root
                .join(ryeos_engine::AI_DIR)
                .join("node/schedules")
        }

        /// Stage one project snapshot declaring `schedules` as
        /// `(schedule_id, interval seconds)` pairs.
        fn stage(&self, label: &str, schedules: &[(&str, u32)]) -> PathBuf {
            let staging = self.root.path().join(format!("staging-{label}"));
            let declarations = staging.join(".ai/config/schedules");
            fs::create_dir_all(&declarations).unwrap();
            let mut body = String::from(
                "category: schedules\nversion: 1.0.0\nschema_version: 1.0.0\nschedules:\n",
            );
            for (schedule_id, interval) in schedules {
                body.push_str(&format!(
                    r#"  - schedule_id: {schedule_id}
    item_ref: graph:project/{schedule_id}
    ref_bindings: {{}}
    schedule_type: interval
    expression: "{interval}"
    timezone: UTC
    misfire_policy: skip
    overlap_policy: skip
    lateness_grace_secs: 60
    enabled: true
    capabilities:
      - ryeos.execute.graph.project/{schedule_id}
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
        kind: live_direct
        access: read_write
        child_policy:
          kind: inherit
    project_root: {project}
    params: {{}}
"#,
                    project = self.project.display(),
                ));
            }
            fs::write(declarations.join("project.yaml"), body).unwrap();
            staging
        }

        fn request(&self, staging: PathBuf, snapshot: &str) -> ProjectScheduleDeployRequest {
            ProjectScheduleDeployRequest {
                staging_root: staging,
                project_path: self.project.clone(),
                project_key: lillux::cas::sha256_hex(b"project-deploy-test-project"),
                snapshot_hash: lillux::cas::sha256_hex(snapshot.as_bytes()),
                caller: Arc::new(GrantedCaller {
                    identity: Arc::clone(&self.identity),
                }),
            }
        }

        /// Exact source bytes and projected record for every observed id.
        fn observe(&self, db: &ScheduleDb) -> Observation {
            OBSERVED
                .iter()
                .map(|schedule_id| {
                    let path = self.schedules_dir().join(format!("{schedule_id}.yaml"));
                    let bytes = match fs::read(&path) {
                        Ok(bytes) => Some(bytes),
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                        Err(error) => panic!("read {}: {error}", path.display()),
                    };
                    let record = db
                        .get_spec(schedule_id)
                        .unwrap()
                        .map(|record| serde_json::to_value(record).unwrap());
                    ((*schedule_id).to_owned(), (bytes, record))
                })
                .collect()
        }

        /// The first deployment: ALPHA and BRAVO exist, CHARLIE does not.
        async fn deploy_baseline(&self, node: &Node) -> Observation {
            let mut session = node.service.acquire_project_schedules().await.unwrap();
            session
                .plan(self.request(self.stage("baseline", &[(ALPHA, 60), (BRAVO, 120)]), "one"))
                .unwrap();
            session.prepare_recovery().await.unwrap();
            let report = session.prepare().unwrap();
            assert_eq!((report.created, report.updated, report.deleted), (2, 0, 0));
            session.finalize().unwrap();
            drop(session);
            let baseline = self.observe(&node.db);
            assert!(baseline[ALPHA].0.is_some() && baseline[ALPHA].1.is_some());
            assert!(baseline[BRAVO].0.is_some() && baseline[BRAVO].1.is_some());
            assert_eq!(baseline[CHARLIE], (None, None));
            baseline
        }

        /// The second deployment, prepared but unresolved: ALPHA is updated,
        /// BRAVO is no longer declared and is deleted, CHARLIE is created.
        async fn prepare_second(&self, node: &Node) -> Box<dyn ProjectScheduleSession> {
            let mut session = node.service.acquire_project_schedules().await.unwrap();
            session
                .plan(self.request(self.stage("second", &[(ALPHA, 300), (CHARLIE, 90)]), "two"))
                .unwrap();
            assert!(session.has_actions().unwrap());
            session.prepare_recovery().await.unwrap();
            let report = session.prepare().unwrap();
            assert_eq!((report.created, report.updated, report.deleted), (1, 1, 1));
            session
        }
    }

    fn assert_second_applied(applied: &Observation, baseline: &Observation) {
        assert_ne!(
            applied[ALPHA].0, baseline[ALPHA].0,
            "update rewrote the source"
        );
        assert_ne!(
            applied[ALPHA].1, baseline[ALPHA].1,
            "update reprojected the record"
        );
        assert_eq!(
            applied[BRAVO],
            (None, None),
            "delete removed source and record"
        );
        assert!(applied[CHARLIE].0.is_some() && applied[CHARLIE].1.is_some());
    }

    /// The journal carries before-images as canonical JSON; decode them back
    /// through that exact encoding so restoration sees what a restart reads.
    fn journal_round_trip(
        images: &[ScheduleRecoveryBeforeImage],
    ) -> Vec<ScheduleRecoveryBeforeImage> {
        let encoded = lillux::canonical_json(&serde_json::to_value(images).unwrap()).unwrap();
        serde_json::from_str(&encoded).unwrap()
    }

    fn recovery_pending(node: &Node) -> bool {
        node.service
            .project_recovery_pending
            .load(Ordering::Acquire)
    }

    #[tokio::test]
    async fn before_images_are_exact_signed_source_bytes_and_record_absence() {
        let host = TestSchedulerHost::acquire();
        let fixture = Fixture::new();
        let node = fixture.open(&host);
        let baseline = fixture.deploy_baseline(&node).await;

        let mut session = node.service.acquire_project_schedules().await.unwrap();
        session
            .plan(fixture.request(
                fixture.stage("second", &[(ALPHA, 300), (CHARLIE, 90)]),
                "two",
            ))
            .unwrap();
        assert!(
            session.before_images().is_err(),
            "images exist only after preparation"
        );
        session.prepare_recovery().await.unwrap();
        let images = session.before_images().unwrap().to_vec();

        let ids = images
            .iter()
            .map(|image| image.schedule_id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            ids,
            vec![ALPHA, BRAVO, CHARLIE],
            "sorted, one image per touched id"
        );
        for image in &images {
            let expected = baseline[&image.schedule_id]
                .0
                .as_ref()
                .map(|bytes| String::from_utf8(bytes.clone()).unwrap());
            assert_eq!(image.signed_yaml, expected, "{} image", image.schedule_id);
        }
        assert!(
            images[2].signed_yaml.is_none(),
            "a created schedule's image is absence"
        );
        assert_eq!(journal_round_trip(&images).len(), images.len());
        for (decoded, original) in journal_round_trip(&images).iter().zip(&images) {
            assert_eq!(decoded.signed_yaml, original.signed_yaml);
        }

        // Capture alone mutates nothing.
        assert_eq!(fixture.observe(&node.db), baseline);
        session.rollback().unwrap();
        assert_eq!(fixture.observe(&node.db), baseline);
        drop(session);
        fixture.close(&host, node);
    }

    #[tokio::test]
    async fn rollback_restores_created_updated_and_deleted_schedules_exactly() {
        let host = TestSchedulerHost::acquire();
        let fixture = Fixture::new();
        let node = fixture.open(&host);
        let baseline = fixture.deploy_baseline(&node).await;

        let mut session = fixture.prepare_second(&node).await;
        assert_second_applied(&fixture.observe(&node.db), &baseline);
        session.rollback().unwrap();
        assert_eq!(fixture.observe(&node.db), baseline);
        assert!(!recovery_pending(&node));
        // Repeating a completed rollback is a no-op, not a second mutation.
        session.rollback().unwrap();
        assert_eq!(fixture.observe(&node.db), baseline);
        drop(session);
        assert!(
            !recovery_pending(&node),
            "a rolled-back session leaves no obligation"
        );

        // The node accepts the next deployment without residue.
        let mut next = node.service.acquire_project_schedules().await.unwrap();
        let report = next
            .plan(fixture.request(fixture.stage("again", &[(ALPHA, 60), (BRAVO, 120)]), "one"))
            .unwrap();
        assert_eq!(
            report.declared, 2,
            "the restored node plans the next deployment"
        );
        drop(next);
        fixture.close(&host, node);
    }

    #[tokio::test]
    async fn restart_restores_journal_images_after_the_store_is_reopened() {
        let host = TestSchedulerHost::acquire();
        let fixture = Fixture::new();
        let node = fixture.open(&host);
        let baseline = fixture.deploy_baseline(&node).await;

        let mut session = fixture.prepare_second(&node).await;
        let journal = journal_round_trip(session.before_images().unwrap());
        session.retain_for_recovery().unwrap();
        drop(session);
        assert!(recovery_pending(&node));
        let applied = fixture.observe(&node.db);
        assert_second_applied(&applied, &baseline);
        fixture.close(&host, node);

        // A new process: a reopened store, a fresh service and session. Only
        // the journal's images say what existed before the interrupted apply.
        let node = fixture.reopen(&host);
        assert_eq!(
            fixture.observe(&node.db),
            applied,
            "the reopened store kept the partial apply"
        );
        let mut session = node.service.acquire_project_schedules().await.unwrap();
        session.prepare_restoration(&journal).await.unwrap();
        session.restore().unwrap();
        assert_eq!(fixture.observe(&node.db), baseline);
        assert!(!recovery_pending(&node));
        // Restoration is idempotent for a predecessor that crashed after it.
        session.restore().unwrap();
        assert_eq!(fixture.observe(&node.db), baseline);
        drop(session);

        let mut next = node.service.acquire_project_schedules().await.unwrap();
        let report = next
            .plan(fixture.request(fixture.stage("again", &[(ALPHA, 60), (BRAVO, 120)]), "one"))
            .unwrap();
        assert_eq!(
            report.declared, 2,
            "the restored node plans the next deployment"
        );
        drop(next);
        fixture.close(&host, node);
    }

    #[tokio::test]
    async fn restart_commit_resolution_keeps_the_applied_target() {
        let host = TestSchedulerHost::acquire();
        let fixture = Fixture::new();
        let node = fixture.open(&host);
        let baseline = fixture.deploy_baseline(&node).await;

        let mut session = fixture.prepare_second(&node).await;
        let journal = journal_round_trip(session.before_images().unwrap());
        session.retain_for_recovery().unwrap();
        drop(session);
        let applied = fixture.observe(&node.db);
        assert_second_applied(&applied, &baseline);
        fixture.close(&host, node);

        let node = fixture.reopen(&host);
        let mut session = node.service.acquire_project_schedules().await.unwrap();
        session.prepare_restoration(&journal).await.unwrap();
        session.finalize().unwrap();
        assert_eq!(
            fixture.observe(&node.db),
            applied,
            "commit keeps the published target"
        );
        drop(session);
        assert!(!recovery_pending(&node));
        fixture.close(&host, node);
    }

    #[tokio::test]
    async fn restoration_refuses_unsorted_or_mismatched_journal_images() {
        let host = TestSchedulerHost::acquire();
        let fixture = Fixture::new();
        let node = fixture.open(&host);
        let baseline = fixture.deploy_baseline(&node).await;

        let mut session = fixture.prepare_second(&node).await;
        let images = session.before_images().unwrap().to_vec();
        let mut altered = images.clone();
        altered[0].signed_yaml = Some("# not the captured source\n".to_owned());
        assert!(
            session.prepare_restoration(&altered).await.is_err(),
            "the retained session refuses images it did not capture"
        );
        session.retain_for_recovery().unwrap();
        drop(session);
        fixture.close(&host, node);

        let node = fixture.reopen(&host);
        let mut reversed = images.clone();
        reversed.reverse();
        let mut session = node.service.acquire_project_schedules().await.unwrap();
        assert!(session.prepare_restoration(&reversed).await.is_err());
        drop(session);
        let mut session = node.service.acquire_project_schedules().await.unwrap();
        session.prepare_restoration(&images).await.unwrap();
        session.restore().unwrap();
        assert_eq!(fixture.observe(&node.db), baseline);
        drop(session);
        fixture.close(&host, node);
    }

    #[tokio::test]
    async fn dropped_unresolved_session_keeps_the_recovery_owner_until_restore() {
        let host = TestSchedulerHost::acquire();
        let fixture = Fixture::new();
        let node = fixture.open(&host);
        let baseline = fixture.deploy_baseline(&node).await;

        let session = fixture.prepare_second(&node).await;
        let images = session.before_images().unwrap().to_vec();
        drop(session);
        assert!(
            recovery_pending(&node),
            "dropping a prepared session never implies success"
        );

        let refused = match node.service.try_acquire_project_schedules() {
            Ok(_) => panic!("pending recovery must refuse a new mutation session"),
            Err(error) => format!("{error:#}"),
        };
        assert!(refused.contains("unresolved recovery"), "got: {refused}");
        let mut blocked = node.service.acquire_project_schedules().await.unwrap();
        let refused = blocked
            .plan(fixture.request(fixture.stage("blocked", &[(ALPHA, 60)]), "three"))
            .expect_err("planning is blocked until the obligation resolves");
        assert!(format!("{refused:#}").contains("unresolved recovery"));
        assert_second_applied(&fixture.observe(&node.db), &baseline);

        blocked.prepare_restoration(&images).await.unwrap();
        blocked.restore().unwrap();
        assert_eq!(fixture.observe(&node.db), baseline);
        assert!(!recovery_pending(&node));
        drop(blocked);
        assert!(
            node.service
                .try_acquire_project_schedules()
                .unwrap()
                .is_some()
        );
        fixture.close(&host, node);
    }

    #[tokio::test]
    async fn rollback_failure_keeps_the_same_owner_until_restore_succeeds() {
        let host = TestSchedulerHost::acquire();
        let fixture = Fixture::new();
        let node = fixture.open(&host);
        let baseline = fixture.deploy_baseline(&node).await;

        let mut session = fixture.prepare_second(&node).await;
        let images = session.before_images().unwrap().to_vec();
        // Occupy ALPHA's source name with a non-regular entry so neither
        // rollback nor restoration can rewrite it.
        let alpha = fixture.schedules_dir().join(format!("{ALPHA}.yaml"));
        fs::remove_file(&alpha).unwrap();
        fs::create_dir(&alpha).unwrap();

        assert!(session.rollback().is_err());
        assert!(
            recovery_pending(&node),
            "a failed rollback keeps the obligation"
        );
        assert!(
            matches!(node.service.try_acquire_project_schedules(), Ok(None)),
            "the failed session still holds the scheduler gate"
        );
        session.prepare_restoration(&images).await.unwrap();
        assert!(session.restore().is_err());
        assert!(recovery_pending(&node));

        fs::remove_dir(&alpha).unwrap();
        session.restore().unwrap();
        assert_eq!(fixture.observe(&node.db), baseline);
        assert!(!recovery_pending(&node));
        drop(session);
        fixture.close(&host, node);
    }

    /// How a planned source is disturbed after its before-image is captured.
    #[derive(Clone, Copy, Debug)]
    enum Tamper {
        /// A different body, validly re-signed by the trusted node key.
        Resigned,
        /// The same body re-signed at another time: same spec hash, new bytes.
        SameBodyResigned,
        /// A different body under the original, now invalid signature.
        InvalidSignature,
        /// The exact original bytes, reachable only through a symlink.
        Symlink,
        /// A non-regular entry at the source name.
        Directory,
        /// The source removed.
        Removed,
        /// A valid trusted source placed where none existed.
        Planted,
    }

    impl Fixture {
        /// Plan the second deployment without preparing anything.
        async fn plan_second(&self, node: &Node) -> Box<dyn ProjectScheduleSession> {
            let mut session = node.service.acquire_project_schedules().await.unwrap();
            session
                .plan(self.request(self.stage("second", &[(ALPHA, 300), (CHARLIE, 90)]), "two"))
                .unwrap();
            session
        }

        fn sign(&self, body: &str, signed_at: &str) -> String {
            lillux::signature::sign_content_at(
                body,
                self.identity.signing_key(),
                "#",
                None,
                signed_at,
            )
        }

        fn tamper(&self, schedule_id: &str, tamper: Tamper) {
            let name = format!("{schedule_id}.yaml");
            let path = self.schedules_dir().join(&name);
            if let Tamper::Planted = tamper {
                // A trusted source for this id, derived from ALPHA's.
                let alpha =
                    fs::read_to_string(self.schedules_dir().join(format!("{ALPHA}.yaml"))).unwrap();
                let body =
                    lillux::signature::strip_signature_lines(&alpha).replace(ALPHA, schedule_id);
                fs::write(&path, self.sign(&body, "2001-01-01T00:00:00Z")).unwrap();
                return;
            }
            let original = fs::read_to_string(&path).unwrap();
            let body = lillux::signature::strip_signature_lines(&original);
            let changed_body = body.replace("enabled: true", "enabled: false");
            assert_ne!(changed_body, body, "fixture source declares enabled: true");
            match tamper {
                Tamper::Resigned => {
                    fs::write(&path, self.sign(&changed_body, "2001-01-01T00:00:00Z")).unwrap();
                }
                Tamper::SameBodyResigned => {
                    let resigned = self.sign(&body, "2001-01-01T00:00:00Z");
                    assert_ne!(resigned, original, "re-signing changed only the signature");
                    fs::write(&path, resigned).unwrap();
                }
                Tamper::InvalidSignature => {
                    let signature_line = original.lines().next().unwrap();
                    assert!(signature_line.starts_with('#'));
                    fs::write(&path, format!("{signature_line}\n{changed_body}")).unwrap();
                }
                Tamper::Symlink => {
                    let elsewhere = self.root.path().join(format!("elsewhere-{name}"));
                    fs::write(&elsewhere, &original).unwrap();
                    fs::remove_file(&path).unwrap();
                    lillux::PinnedDirectory::open(&self.schedules_dir())
                        .unwrap()
                        .unwrap()
                        .create_symlink(
                            std::ffi::OsStr::new(&name),
                            elsewhere.to_str().unwrap().as_bytes(),
                        )
                        .unwrap();
                }
                Tamper::Directory => {
                    fs::remove_file(&path).unwrap();
                    fs::create_dir(&path).unwrap();
                }
                Tamper::Removed => fs::remove_file(&path).unwrap(),
                Tamper::Planted => unreachable!("handled above"),
            }
        }
    }

    /// The disturbed entry exactly as it now stands on disk, if present.
    fn entry_state(path: &Path) -> Option<(bool, bool, Option<Vec<u8>>)> {
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
            Err(error) => panic!("inspect {}: {error}", path.display()),
        };
        let bytes = metadata
            .file_type()
            .is_file()
            .then(|| fs::read(path).unwrap());
        Some((
            metadata.file_type().is_symlink(),
            metadata.file_type().is_dir(),
            bytes,
        ))
    }

    /// Disturb `schedule_id` after before-image capture (so preparation's own
    /// revalidation under the lock must refuse) or before it (so capture
    /// refuses). The refusal must name `expected`, and neither the disturbed
    /// entry nor the projected record may change.
    async fn assert_tampered_source_refused(
        schedule_id: &str,
        tamper: Tamper,
        after_capture: bool,
        expected: &str,
    ) {
        let host = TestSchedulerHost::acquire();
        let fixture = Fixture::new();
        let node = fixture.open(&host);
        let baseline = fixture.deploy_baseline(&node).await;
        let path = fixture.schedules_dir().join(format!("{schedule_id}.yaml"));

        let mut session = fixture.plan_second(&node).await;
        let error = if after_capture {
            session.prepare_recovery().await.unwrap();
            fixture.tamper(schedule_id, tamper);
            session.prepare().map(|_| ())
        } else {
            fixture.tamper(schedule_id, tamper);
            session.prepare_recovery().await
        };
        let disturbed = entry_state(&path);
        let error = match error {
            Ok(()) => panic!("{tamper:?} {schedule_id} source must be refused"),
            Err(error) => format!("{error:#}"),
        };
        assert!(
            error.contains(expected),
            "{tamper:?} {schedule_id}: expected {expected:?}, got: {error}"
        );
        assert_eq!(
            entry_state(&path),
            disturbed,
            "{tamper:?}: the refused deploy never rewrote or removed the entry"
        );
        let record = node
            .db
            .get_spec(schedule_id)
            .unwrap()
            .map(|record| serde_json::to_value(record).unwrap());
        assert_eq!(
            record, baseline[schedule_id].1,
            "{tamper:?}: the record is unchanged"
        );
        drop(session);
        fixture.close(&host, node);
    }

    #[tokio::test]
    async fn update_refuses_a_resigned_different_source() {
        assert_tampered_source_refused(ALPHA, Tamper::Resigned, true, "YAML changed").await;
    }

    #[tokio::test]
    async fn update_refuses_a_same_body_resign_after_capture() {
        assert_tampered_source_refused(
            ALPHA,
            Tamper::SameBodyResigned,
            true,
            "changed after its recovery before-image was captured",
        )
        .await;
    }

    #[tokio::test]
    async fn update_refuses_a_source_with_an_invalid_signature() {
        assert_tampered_source_refused(
            ALPHA,
            Tamper::InvalidSignature,
            true,
            "verify existing schedule YAML",
        )
        .await;
    }

    // A symlink or directory placed after capture is refused by the
    // directory-wide scan that opens preparation under the lock, before any
    // per-action read; the scan's own refusal text proves which check fired.
    // The pinned per-action open refuses the same entries (shown by the
    // capture tests below) only if one appears after that scan.
    #[tokio::test]
    async fn locked_scan_refuses_a_symlinked_source_after_capture() {
        assert_tampered_source_refused(ALPHA, Tamper::Symlink, true, LOCKED_SCAN_REFUSAL).await;
    }

    #[tokio::test]
    async fn locked_scan_refuses_a_non_regular_source_after_capture() {
        assert_tampered_source_refused(ALPHA, Tamper::Directory, true, LOCKED_SCAN_REFUSAL).await;
    }

    #[tokio::test]
    async fn capture_refuses_a_symlinked_source() {
        assert_tampered_source_refused(ALPHA, Tamper::Symlink, false, "open secure regular file")
            .await;
    }

    #[tokio::test]
    async fn capture_refuses_a_non_regular_source() {
        assert_tampered_source_refused(
            ALPHA,
            Tamper::Directory,
            false,
            "secure input is not a regular file",
        )
        .await;
    }

    #[tokio::test]
    async fn delete_refuses_a_resigned_different_source() {
        assert_tampered_source_refused(BRAVO, Tamper::Resigned, true, "YAML changed").await;
    }

    #[tokio::test]
    async fn delete_refuses_a_missing_source() {
        assert_tampered_source_refused(BRAVO, Tamper::Removed, true, "lost node YAML").await;
    }

    #[tokio::test]
    async fn create_refuses_a_present_verifying_source() {
        assert_tampered_source_refused(CHARLIE, Tamper::Planted, true, "gained node YAML").await;
    }

    #[tokio::test]
    async fn rollback_after_finalize_is_refused_and_keeps_the_committed_target() {
        let host = TestSchedulerHost::acquire();
        let fixture = Fixture::new();
        let node = fixture.open(&host);
        let baseline = fixture.deploy_baseline(&node).await;

        let mut session = fixture.prepare_second(&node).await;
        session.finalize().unwrap();
        let applied = fixture.observe(&node.db);
        assert_second_applied(&applied, &baseline);
        let error = session
            .rollback()
            .expect_err("a finalized deploy is committed and cannot be reverted");
        assert!(
            format!("{error:#}").contains("cannot rollback"),
            "got: {error:#}"
        );
        assert_eq!(fixture.observe(&node.db), applied);
        assert!(!recovery_pending(&node));
        drop(session);
        fixture.close(&host, node);
    }

    /// A stray entry at a name no planned action reads: only the locked
    /// directory scan can refuse it.
    #[tokio::test]
    async fn locked_scan_refuses_a_stray_entry_no_action_reads() {
        let host = TestSchedulerHost::acquire();
        let fixture = Fixture::new();
        let node = fixture.open(&host);
        let baseline = fixture.deploy_baseline(&node).await;

        let mut session = fixture.plan_second(&node).await;
        session.prepare_recovery().await.unwrap();
        let stray = fixture.schedules_dir().join("operator-notes.txt");
        fs::write(&stray, "not a schedule source\n").unwrap();
        let error = session
            .prepare()
            .expect_err("a stray entry must refuse preparation");
        assert!(
            format!("{error:#}").contains(LOCKED_SCAN_REFUSAL),
            "got: {error:#}"
        );
        assert_eq!(fixture.observe(&node.db), baseline, "nothing was applied");
        assert_eq!(fs::read(&stray).unwrap(), b"not a schedule source\n");
        drop(session);
        fixture.close(&host, node);
    }

    /// A create whose captured image is a present source must still find that
    /// source: removing it after capture would make rollback and journal
    /// restoration disagree, so preparation refuses.
    #[tokio::test]
    async fn create_refuses_a_source_removed_after_its_image_was_captured() {
        let host = TestSchedulerHost::acquire();
        let fixture = Fixture::new();
        let node = fixture.open(&host);
        fixture.deploy_baseline(&node).await;

        let mut session = fixture.plan_second(&node).await;
        fixture.tamper(CHARLIE, Tamper::Planted);
        session.prepare_recovery().await.unwrap();
        let captured = session
            .before_images()
            .unwrap()
            .iter()
            .find(|image| image.schedule_id == CHARLIE)
            .and_then(|image| image.signed_yaml.clone());
        assert!(captured.is_some(), "capture recorded the planted source");
        let path = fixture.schedules_dir().join(format!("{CHARLIE}.yaml"));
        fs::remove_file(&path).unwrap();

        let error = session
            .prepare()
            .expect_err("a source removed after capture must refuse preparation");
        assert!(
            format!("{error:#}").contains("changed after its recovery before-image was captured"),
            "got: {error:#}"
        );
        assert!(entry_state(&path).is_none(), "the source stays absent");
        assert!(
            node.db.get_spec(CHARLIE).unwrap().is_none(),
            "no record was projected"
        );
        drop(session);
        fixture.close(&host, node);
    }

    /// The per-schedule fire history directory a deploy never owns.
    fn fire_history(fixture: &Fixture, schedule_id: &str) -> PathBuf {
        fixture
            .app_root
            .join(ryeos_engine::AI_DIR)
            .join("state")
            .join("schedules")
            .join(schedule_id)
    }

    #[tokio::test]
    async fn create_refuses_an_id_whose_fire_history_is_retained() {
        let host = TestSchedulerHost::acquire();
        let fixture = Fixture::new();
        let node = fixture.open(&host);
        let baseline = fixture.deploy_baseline(&node).await;
        let history = fire_history(&fixture, CHARLIE);
        fs::create_dir_all(&history).unwrap();

        let mut session = node.service.acquire_project_schedules().await.unwrap();
        let error = session
            .plan(fixture.request(
                fixture.stage("second", &[(ALPHA, 300), (CHARLIE, 90)]),
                "two",
            ))
            .expect_err("a new schedule must not inherit a retired id's fire history");
        assert!(
            format!("{error:#}").contains("reuse not allowed"),
            "got: {error:#}"
        );
        assert_eq!(fixture.observe(&node.db), baseline, "refusal mutates nothing");
        assert!(history.is_dir(), "the retained history is untouched");
        drop(session);
        fixture.close(&host, node);
    }

    #[tokio::test]
    async fn omission_and_rollback_keep_the_omitted_schedules_fire_history() {
        let host = TestSchedulerHost::acquire();
        let fixture = Fixture::new();
        let node = fixture.open(&host);
        let baseline = fixture.deploy_baseline(&node).await;
        let history = fire_history(&fixture, BRAVO);
        fs::create_dir_all(&history).unwrap();
        fs::write(history.join("fire.marker"), b"retained").unwrap();

        let mut session = fixture.prepare_second(&node).await;
        assert_eq!(fixture.observe(&node.db)[BRAVO], (None, None));
        assert_eq!(
            fs::read(history.join("fire.marker")).unwrap(),
            b"retained",
            "omission deletes the schedule, never its fire history"
        );
        session.rollback().unwrap();
        assert_eq!(fixture.observe(&node.db), baseline);
        assert_eq!(fs::read(history.join("fire.marker")).unwrap(), b"retained");
        drop(session);
        fixture.close(&host, node);
    }

    #[tokio::test]
    async fn omitting_every_declaration_deletes_all_and_rollback_restores_them() {
        let host = TestSchedulerHost::acquire();
        let fixture = Fixture::new();
        let node = fixture.open(&host);
        let baseline = fixture.deploy_baseline(&node).await;
        let histories: Vec<PathBuf> = [ALPHA, BRAVO]
            .iter()
            .map(|schedule_id| {
                let history = fire_history(&fixture, schedule_id);
                fs::create_dir_all(&history).unwrap();
                fs::write(history.join("fire.marker"), b"retained").unwrap();
                history
            })
            .collect();

        let staging = fixture.stage("empty", &[(ALPHA, 300)]);
        fs::remove_file(staging.join(".ai/config/schedules/project.yaml")).unwrap();
        let mut session = node.service.acquire_project_schedules().await.unwrap();
        session.plan(fixture.request(staging, "empty")).unwrap();
        assert!(session.has_actions().unwrap());
        session.prepare_recovery().await.unwrap();
        let report = session.prepare().unwrap();
        assert_eq!((report.created, report.updated, report.deleted), (0, 0, 2));
        let emptied = fixture.observe(&node.db);
        assert_eq!(emptied[ALPHA], (None, None));
        assert_eq!(emptied[BRAVO], (None, None));
        for history in &histories {
            assert_eq!(fs::read(history.join("fire.marker")).unwrap(), b"retained");
        }
        session.rollback().unwrap();
        assert_eq!(fixture.observe(&node.db), baseline);
        for history in &histories {
            assert_eq!(fs::read(history.join("fire.marker")).unwrap(), b"retained");
        }
        drop(session);
        fixture.close(&host, node);
    }
}
