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
    apply_actions(plan, ctx, &mut preparation.tx)
}

fn apply_actions(
    plan: &ScheduleDeployPlan,
    ctx: &ProjectContext,
    tx: &mut ScheduleReconcileTx,
) -> Result<ScheduleDeployReport> {
    let mut report = ScheduleDeployReport {
        declared: plan.declared,
        ..ScheduleDeployReport::default()
    };

    for action in &plan.actions {
        revalidate_action(action, ctx, tx.directory.path())?;
        match action {
            ScheduleAction::Create(desired) => {
                tx.backup(ctx, &desired.declaration.schedule_id)?;
                write_reconciled_schedule(
                    &tx.directory,
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
                tx.backup(ctx, &desired.declaration.schedule_id)?;
                write_reconciled_schedule(&tx.directory, desired, ctx, existing.registered_at)?;
                tx.touch(desired.declaration.schedule_id.clone());
                report.updated += 1;
            }
            ScheduleAction::DeleteMissing {
                schedule_id,
                existing: _,
            } => {
                tx.backup(ctx, schedule_id)?;
                let yaml_path = tx.schedule_path(schedule_id);
                let name = yaml_path
                    .file_name()
                    .ok_or_else(|| anyhow!("schedule path has no filename"))?;
                if let Some(file) = tx.directory.open_regular(name, false)? {
                    tx.directory
                        .remove_if_same(name, &file)
                        .with_context(|| format!("delete schedule YAML {}", yaml_path.display()))?;
                }
                ctx.db.delete_spec(schedule_id)?;
                tx.touch(schedule_id.clone());
                report.deleted += 1;
            }
        }
    }
    Ok(report)
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

fn revalidate_action(
    action: &ScheduleAction,
    ctx: &ProjectContext,
    schedules_dir: &Path,
) -> Result<()> {
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
            if read_existing_schedule_body(schedules_dir, schedule_id, &ctx.trust_store)?.is_some()
            {
                anyhow::bail!(
                    "schedule_id '{}' gained node YAML during project deploy; refusing to overwrite",
                    schedule_id
                );
            }
            reject_schedule_history_reuse(ctx, schedule_id)?;
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
            let body = read_existing_schedule_body(schedules_dir, schedule_id, &ctx.trust_store)?
                .ok_or_else(|| {
                anyhow!(
                    "schedule_id '{}' lost node YAML during project deploy; refusing update",
                    schedule_id
                )
            })?;
            let yaml_hash = read_existing_schedule_content_hash(schedules_dir, schedule_id)?
                .ok_or_else(|| {
                    anyhow!(
                        "schedule_id '{}' lost node YAML during project deploy",
                        schedule_id
                    )
                })?;
            if yaml_hash != existing.spec_hash {
                anyhow::bail!(
                    "schedule_id '{}' YAML changed during project deploy; retry project sync",
                    schedule_id
                );
            }
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
        }
        ScheduleAction::DeleteMissing {
            schedule_id,
            existing,
        } => {
            let current = ctx
                .db
                .get_spec(schedule_id)?
                .ok_or_else(|| {
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
            let body = read_existing_schedule_body(schedules_dir, schedule_id, &ctx.trust_store)?
                .ok_or_else(|| {
                anyhow!(
                    "schedule_id '{}' lost node YAML during project deploy; refusing delete",
                    schedule_id
                )
            })?;
            let yaml_hash = read_existing_schedule_content_hash(schedules_dir, schedule_id)?
                .ok_or_else(|| {
                    anyhow!(
                        "schedule_id '{}' lost node YAML during project deploy",
                        schedule_id
                    )
                })?;
            if yaml_hash != existing.spec_hash {
                anyhow::bail!(
                    "schedule_id '{}' YAML changed during project deploy; refusing delete",
                    schedule_id
                );
            }
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
        }
    }
    Ok(())
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

fn read_existing_schedule_content_hash(
    schedules_dir: &Path,
    schedule_id: &str,
) -> Result<Option<String>> {
    let yaml_path = canonical_project_schedule_path(schedules_dir, schedule_id)?;
    match fs::symlink_metadata(&yaml_path) {
        Ok(metadata) if metadata.file_type().is_file() && !metadata.file_type().is_symlink() => {}
        Ok(_) => {
            anyhow::bail!(
                "schedule source {} is not a regular non-symlink file",
                yaml_path.display()
            );
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(None);
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("inspect schedule source {}", yaml_path.display()));
        }
    }
    let content = fs::read(&yaml_path)
        .with_context(|| format!("read existing schedule YAML {}", yaml_path.display()))?;
    Ok(Some(lillux::cas::sha256_hex(&content)))
}

#[allow(clippy::too_many_arguments)]
fn write_reconciled_schedule(
    schedules_dir: &lillux::PinnedDirectory,
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
    let expected = schedules_dir.open_regular(filename, false)?;
    schedules_dir.atomic_write_if_same(
        filename,
        expected.as_ref(),
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

    fn backup(&mut self, ctx: &ProjectContext, schedule_id: &str) -> Result<()> {
        if self.backups.iter().any(|b| b.schedule_id == schedule_id) {
            return Ok(());
        }
        let yaml_path = self.schedule_path(schedule_id);
        let name = yaml_path
            .file_name()
            .ok_or_else(|| anyhow!("schedule path has no filename"))?;
        let yaml_bytes = self
            .directory
            .open_pinned_regular(name, false)?
            .map(|file| file.read_bounded(MAX_RECOVERY_SCHEDULE_SOURCE_BYTES))
            .transpose()
            .with_context(|| format!("backup {}", yaml_path.display()))?;
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
        if self.phase != SessionPhase::Prepared {
            if !matches!(
                self.phase,
                SessionPhase::RecoveryPrepared | SessionPhase::RecoveryRequired
            ) {
                anyhow::bail!("project schedule session cannot finalize in its current phase");
            }
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
        if !matches!(
            self.phase,
            SessionPhase::Acquired
                | SessionPhase::Planned
                | SessionPhase::RecoveryPrepared
                | SessionPhase::Prepared
                | SessionPhase::RecoveryRequired
                | SessionPhase::Finalized
                | SessionPhase::Restored
                | SessionPhase::RolledBack
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
