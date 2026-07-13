use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use ryeos_app::node_config::writer;
use ryeos_scheduler::types::ScheduleSpecRecord;
use serde::Deserialize;
use serde_json::Value;

use super::ProjectDeployContext;

const MANAGED_BY_TYPE: &str = "project_ai_sync";

#[derive(Debug, Default)]
pub struct ScheduleDeployPlan {
    actions: Vec<ScheduleAction>,
    declared: usize,
}

#[derive(Debug, Clone, Default)]
pub struct ScheduleDeployReport {
    pub declared: usize,
    pub created: usize,
    pub updated: usize,
    pub deleted: usize,
}

#[derive(Debug)]
enum ScheduleAction {
    Create(DesiredSchedule),
    Update {
        desired: DesiredSchedule,
        existing: ScheduleSpecRecord,
        adopt_manual: bool,
    },
    DeleteMissing {
        schedule_id: String,
        existing: ScheduleSpecRecord,
    },
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

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScheduleDeclaration {
    schedule_id: String,
    item_ref: String,
    schedule_type: String,
    expression: String,
    #[serde(default)]
    timezone: Option<String>,
    #[serde(default)]
    misfire_policy: Option<String>,
    #[serde(default)]
    overlap_policy: Option<String>,
    #[serde(default)]
    lateness_grace_secs: Option<i64>,
    #[serde(default = "default_schedule_enabled")]
    enabled: bool,
    #[serde(default)]
    project_root: Option<String>,
    #[serde(default)]
    params: Value,
}

fn default_schedule_enabled() -> bool {
    true
}

#[cfg(test)]
pub(crate) fn validate_declarations_for_test(
    staging_root: &Path,
    project_path: &Path,
) -> Result<usize> {
    Ok(load_desired_schedules(staging_root, project_path)?.len())
}

pub fn plan(ctx: &ProjectDeployContext<'_>) -> Result<ScheduleDeployPlan> {
    let desired = load_desired_schedules(ctx.staging_root, ctx.project_path)?;
    let mut actions = Vec::new();
    let desired_ids: HashSet<String> = desired
        .iter()
        .map(|schedule| schedule.declaration.schedule_id.clone())
        .collect();
    let desired_by_id: HashMap<String, DesiredSchedule> = desired
        .into_iter()
        .map(|schedule| (schedule.declaration.schedule_id.clone(), schedule))
        .collect();

    let node_dir = ctx
        .state
        .config
        .app_root
        .join(ryeos_engine::AI_DIR)
        .join("node");
    let schedules_dir = node_dir.join("schedules");
    let managed_yaml = load_project_managed_schedule_yaml(&schedules_dir, ctx.project_key)?;

    for desired_schedule in desired_by_id.values() {
        let schedule_id = &desired_schedule.declaration.schedule_id;
        let existing = ctx.state.scheduler_db.get_spec(schedule_id)?;
        let existing_body = read_existing_schedule_body(&schedules_dir, schedule_id)?;

        match existing {
            Some(existing) => {
                let adopt_manual = match existing_body.as_ref().and_then(parse_project_managed_by) {
                    Some(managed) => {
                        if managed.project_key != ctx.project_key {
                            anyhow::bail!(
                                "schedule_id '{}' is managed by another project and cannot be updated by this project sync",
                                schedule_id
                            );
                        }
                        false
                    }
                    None => {
                        let existing_project_root = existing.project_root.as_deref();
                        let sync_project_root = ctx.project_path.to_string_lossy();
                        if existing_project_root != Some(sync_project_root.as_ref()) {
                            anyhow::bail!(
                                "schedule_id '{}' already exists as a manual schedule for project_root {:?}; refusing to adopt for project {}",
                                schedule_id,
                                existing_project_root,
                                ctx.project_path.display(),
                            );
                        }
                        true
                    }
                };
                require_project_reconcile_schedule_owner(
                    ctx.caller,
                    schedule_id,
                    &existing.requester_fingerprint,
                )?;
                actions.push(ScheduleAction::Update {
                    desired: desired_schedule.clone(),
                    existing,
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
        let existing = ctx.state.scheduler_db.get_spec(schedule_id)?.ok_or_else(|| {
            anyhow!(
                "project-managed schedule YAML '{}' exists without DB projection; rebuild projection or deregister before project sync",
                schedule_id
            )
        })?;
        require_project_reconcile_schedule_owner(
            ctx.caller,
            schedule_id,
            &existing.requester_fingerprint,
        )?;
        actions.push(ScheduleAction::DeleteMissing {
            schedule_id: schedule_id.clone(),
            existing,
        });
    }

    for existing in ctx.state.scheduler_db.list_specs(false, None)? {
        if desired_ids.contains(&existing.schedule_id)
            || managed_yaml.contains_key(&existing.schedule_id)
        {
            continue;
        }
        if existing.project_root.as_deref() == Some(ctx.project_path.to_string_lossy().as_ref()) {
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

pub fn commit(
    plan: &ScheduleDeployPlan,
    ctx: &ProjectDeployContext<'_>,
) -> Result<ScheduleDeployReport> {
    let mut prepared = prepare_commit(plan, ctx)?;
    let report = prepared.report.clone();
    prepared.finalize(ctx);
    Ok(report)
}

#[derive(Debug)]
pub struct PreparedScheduleDeploy {
    tx: Option<ScheduleReconcileTx>,
    pub report: ScheduleDeployReport,
    finalized: bool,
}

impl PreparedScheduleDeploy {
    pub fn finalize(&mut self, ctx: &ProjectDeployContext<'_>) {
        if let Some(tx) = self.tx.take() {
            tx.finalize(ctx);
        }
        self.finalized = true;
    }

    pub fn rollback(&mut self, ctx: &ProjectDeployContext<'_>) {
        if let Some(tx) = self.tx.take() {
            tx.rollback(ctx);
        }
        self.finalized = true;
    }
}

impl Drop for PreparedScheduleDeploy {
    fn drop(&mut self) {
        debug_assert!(
            self.finalized,
            "prepared schedule deploy dropped unfinalized"
        );
    }
}

pub fn prepare_commit(
    plan: &ScheduleDeployPlan,
    ctx: &ProjectDeployContext<'_>,
) -> Result<PreparedScheduleDeploy> {
    let node_dir = ctx
        .state
        .config
        .app_root
        .join(ryeos_engine::AI_DIR)
        .join("node");
    let schedules_dir = node_dir.join("schedules");
    let mut tx = ScheduleReconcileTx::new(schedules_dir.clone());
    let mut report = ScheduleDeployReport {
        declared: plan.declared,
        ..ScheduleDeployReport::default()
    };

    let result = (|| -> Result<()> {
        for action in &plan.actions {
            revalidate_action(action, ctx, &schedules_dir)?;
            match action {
                ScheduleAction::Create(desired) => {
                    tx.backup(ctx, &desired.declaration.schedule_id)?;
                    write_reconciled_schedule(
                        &node_dir,
                        desired,
                        ctx,
                        lillux::time::timestamp_millis(),
                        &ctx.caller.fingerprint,
                        &ctx.caller.scopes,
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
                    write_reconciled_schedule(
                        &node_dir,
                        desired,
                        ctx,
                        existing.registered_at,
                        &existing.requester_fingerprint,
                        &existing.capabilities,
                    )?;
                    tx.touch(desired.declaration.schedule_id.clone());
                    report.updated += 1;
                }
                ScheduleAction::DeleteMissing {
                    schedule_id,
                    existing: _,
                } => {
                    tx.backup(ctx, schedule_id)?;
                    let yaml_path = tx.schedule_path(schedule_id);
                    if yaml_path.exists() {
                        fs::remove_file(&yaml_path).with_context(|| {
                            format!("delete schedule YAML {}", yaml_path.display())
                        })?;
                    }
                    ctx.state.scheduler_db.delete_spec(schedule_id)?;
                    tx.touch(schedule_id.clone());
                    report.deleted += 1;
                }
            }
        }
        Ok(())
    })();

    match result {
        Ok(()) => Ok(PreparedScheduleDeploy {
            tx: Some(tx),
            report,
            finalized: false,
        }),
        Err(err) => {
            tx.rollback(ctx);
            Err(err)
        }
    }
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
) -> Result<HashMap<String, Value>> {
    let mut out = HashMap::new();
    if !schedules_dir.is_dir() {
        return Ok(out);
    }
    for entry in fs::read_dir(schedules_dir)
        .with_context(|| format!("read schedules dir {}", schedules_dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if !entry.file_type()?.is_file() || path.is_symlink() {
            continue;
        }
        let ext = path.extension().and_then(|e| e.to_str());
        if !matches!(ext, Some("yaml" | "yml")) {
            continue;
        }
        let Some(schedule_id) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let content = fs::read_to_string(&path)
            .with_context(|| format!("read schedule YAML {}", path.display()))?;
        let body = lillux::signature::strip_signature_lines(&content);
        let value: Value = serde_yaml::from_str(&body)
            .with_context(|| format!("parse schedule YAML {}", path.display()))?;
        let Some(managed) = parse_project_managed_by(&value) else {
            continue;
        };
        if managed.project_key == project_key {
            out.insert(schedule_id.to_string(), value);
        }
    }
    Ok(out)
}

fn revalidate_action(
    action: &ScheduleAction,
    ctx: &ProjectDeployContext<'_>,
    schedules_dir: &Path,
) -> Result<()> {
    match action {
        ScheduleAction::Create(desired) => {
            let schedule_id = &desired.declaration.schedule_id;
            if ctx.state.scheduler_db.get_spec(schedule_id)?.is_some() {
                anyhow::bail!(
                    "schedule_id '{}' changed during project deploy; refusing to overwrite existing schedule",
                    schedule_id
                );
            }
            if read_existing_schedule_body(schedules_dir, schedule_id)?.is_some() {
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
            let current = ctx
                .state
                .scheduler_db
                .get_spec(schedule_id)?
                .ok_or_else(|| {
                    anyhow!(
                        "schedule_id '{}' disappeared during project deploy; refusing update",
                        schedule_id
                    )
                })?;
            if current.spec_hash != existing.spec_hash
                || current.registered_at != existing.registered_at
                || current.requester_fingerprint != existing.requester_fingerprint
            {
                anyhow::bail!(
                    "schedule_id '{}' changed during project deploy; retry project sync",
                    schedule_id
                );
            }
            let body =
                read_existing_schedule_body(schedules_dir, schedule_id)?.ok_or_else(|| {
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
                    if managed.project_key != ctx.project_key {
                        anyhow::bail!(
                            "schedule_id '{}' changed project ownership during project deploy",
                            schedule_id
                        );
                    }
                }
                None if *adopt_manual => {
                    if current.project_root.as_deref()
                        != Some(ctx.project_path.to_string_lossy().as_ref())
                    {
                        anyhow::bail!(
                            "schedule_id '{}' changed project_root during project deploy; refusing manual adoption",
                            schedule_id
                        );
                    }
                    require_project_reconcile_schedule_owner(
                        ctx.caller,
                        schedule_id,
                        &current.requester_fingerprint,
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
                .state
                .scheduler_db
                .get_spec(schedule_id)?
                .ok_or_else(|| {
                    anyhow!(
                        "schedule_id '{}' disappeared during project deploy; refusing delete",
                        schedule_id
                    )
                })?;
            if current.spec_hash != existing.spec_hash
                || current.registered_at != existing.registered_at
                || current.requester_fingerprint != existing.requester_fingerprint
            {
                anyhow::bail!(
                    "schedule_id '{}' changed during project deploy; refusing delete",
                    schedule_id
                );
            }
            let body =
                read_existing_schedule_body(schedules_dir, schedule_id)?.ok_or_else(|| {
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
            if managed.project_key != ctx.project_key {
                anyhow::bail!(
                    "schedule_id '{}' changed project ownership during project deploy",
                    schedule_id
                );
            }
            // Same-project is now re-proven (managed_by.project_key matches);
            // only here do we surface an ownership conflict (409), never before
            // the project-scope check.
            require_project_reconcile_schedule_owner(
                ctx.caller,
                schedule_id,
                &current.requester_fingerprint,
            )?;
        }
    }
    Ok(())
}

fn load_desired_schedules(
    staging_root: &Path,
    project_path: &Path,
) -> Result<Vec<DesiredSchedule>> {
    let schedules_root = staging_root.join(".ai/config/schedules");
    if !schedules_root.is_dir() {
        return Ok(Vec::new());
    }

    let canonical_project_path = project_path.to_string_lossy().to_string();
    let mut seen = HashSet::new();
    let mut desired = Vec::new();
    for path in schedule_declaration_files(&schedules_root)? {
        let rel_path = path
            .strip_prefix(staging_root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        let content = fs::read_to_string(&path)
            .with_context(|| format!("read schedule declaration {}", rel_path))?;
        let body = lillux::signature::strip_signature_lines(&content);
        let file: ScheduleDeclarationFile = serde_yaml::from_str(&body)
            .with_context(|| format!("parse schedule declaration {}", rel_path))?;
        validate_schedule_declaration_file(&file, &rel_path)?;
        let source_body_hash = lillux::cas::sha256_hex(body.as_bytes());
        for schedule in file.schedules {
            validate_schedule_declaration(&schedule, &rel_path, &canonical_project_path)?;
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
            if matches!(ext, Some("yaml" | "yml")) {
                files.push(path);
            } else {
                anyhow::bail!(
                    "schedule declaration path '{}' is not YAML; expected .yaml or .yml",
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
    ryeos_scheduler::crontab::validate_schedule_id(&schedule.schedule_id)
        .with_context(|| format!("invalid schedule_id in {}", rel_path))?;
    ryeos_engine::canonical_ref::CanonicalRef::parse(&schedule.item_ref)
        .with_context(|| format!("invalid item_ref for schedule '{}'", schedule.schedule_id))?;
    ryeos_scheduler::crontab::validate_expression(&schedule.schedule_type, &schedule.expression)
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
    let timezone = schedule.timezone.as_deref().unwrap_or("UTC");
    ryeos_scheduler::crontab::validate_timezone(timezone)
        .with_context(|| format!("invalid timezone for schedule '{}'", schedule.schedule_id))?;
    if let Some(ref p) = schedule.overlap_policy {
        if !matches!(p.as_str(), "allow" | "skip" | "cancel_previous") {
            anyhow::bail!(
                "invalid overlap_policy '{}' for schedule '{}'",
                p,
                schedule.schedule_id
            );
        }
    }
    if let Some(ref p) = schedule.misfire_policy {
        if !is_valid_misfire_policy(p) {
            anyhow::bail!(
                "invalid misfire_policy '{}' for schedule '{}'",
                p,
                schedule.schedule_id
            );
        }
    }
    if let Some(secs) = schedule.lateness_grace_secs {
        if secs <= 0 {
            anyhow::bail!(
                "lateness_grace_secs must be positive for schedule '{}'",
                schedule.schedule_id
            );
        }
    }
    if !schedule.params.is_null() && !schedule.params.is_object() {
        anyhow::bail!(
            "params must be a mapping for schedule '{}'",
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

fn is_valid_misfire_policy(p: &str) -> bool {
    match p {
        "skip" | "fire_once_now" => true,
        s if s.starts_with("catch_up_bounded:") => s
            .strip_prefix("catch_up_bounded:")
            .and_then(|n| n.parse::<usize>().ok())
            .is_some(),
        s if s.starts_with("catch_up_within_secs:") => s
            .strip_prefix("catch_up_within_secs:")
            .and_then(|n| n.parse::<u64>().ok())
            .is_some(),
        _ => false,
    }
}

fn require_schedule_registration_authority(ctx: &ProjectDeployContext<'_>) -> Result<()> {
    if ctx.caller.scopes.is_empty() {
        anyhow::bail!(
            "project schedule creation requires verified caller context with non-empty scopes"
        );
    }
    if ctx
        .caller
        .scopes
        .iter()
        .any(|scope| scope == "*" || scope == "ryeos.execute.service.scheduler/register")
    {
        Ok(())
    } else {
        anyhow::bail!(
            "project schedule creation requires ryeos.execute.service.scheduler/register capability"
        );
    }
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
    caller: &crate::handler_context::HandlerContext,
    schedule_id: &str,
    owner_fingerprint: &str,
) -> Result<()> {
    use crate::handler_error::HandlerError;
    if caller.is_owner(Some(owner_fingerprint)) {
        return Ok(());
    }
    if !caller.is_present() {
        return Err(anyhow!(HandlerError::NotFound));
    }
    Err(anyhow!(HandlerError::Conflict(format!(
        "schedule '{schedule_id}' in this project is registered by a different \
         principal; deregister it on the remote or run the sync as its owner"
    ))))
}

fn reject_schedule_history_reuse(ctx: &ProjectDeployContext<'_>, schedule_id: &str) -> Result<()> {
    let fires_dir = ctx
        .state
        .config
        .app_root
        .join(ryeos_engine::AI_DIR)
        .join("state")
        .join("schedules")
        .join(schedule_id);
    if fires_dir.exists() {
        anyhow::bail!(
            "schedule_id '{}' reuse not allowed: fire history exists at {} — deregister first or use a different ID",
            schedule_id,
            fires_dir.display()
        );
    }
    Ok(())
}

fn read_existing_schedule_body(schedules_dir: &Path, schedule_id: &str) -> Result<Option<Value>> {
    let yaml_path = schedules_dir.join(format!("{schedule_id}.yaml"));
    if !yaml_path.exists() {
        return Ok(None);
    }
    let content = fs::read_to_string(&yaml_path)
        .with_context(|| format!("read existing schedule YAML {}", yaml_path.display()))?;
    let body = lillux::signature::strip_signature_lines(&content);
    let value = serde_yaml::from_str(&body)
        .with_context(|| format!("parse existing schedule YAML {}", yaml_path.display()))?;
    Ok(Some(value))
}

fn read_existing_schedule_content_hash(
    schedules_dir: &Path,
    schedule_id: &str,
) -> Result<Option<String>> {
    let yaml_path = schedules_dir.join(format!("{schedule_id}.yaml"));
    if !yaml_path.exists() {
        return Ok(None);
    }
    let content = fs::read(&yaml_path)
        .with_context(|| format!("read existing schedule YAML {}", yaml_path.display()))?;
    Ok(Some(lillux::cas::sha256_hex(&content)))
}

#[allow(clippy::too_many_arguments)]
fn write_reconciled_schedule(
    node_dir: &Path,
    desired: &DesiredSchedule,
    ctx: &ProjectDeployContext<'_>,
    registered_at: i64,
    requester_fingerprint: &str,
    capabilities: &[String],
) -> Result<()> {
    if requester_fingerprint.is_empty() || capabilities.is_empty() {
        anyhow::bail!(
            "project schedule '{}' cannot be reconciled without execution requester and capabilities",
            desired.declaration.schedule_id
        );
    }

    let schedule = &desired.declaration;
    let canonical_project_path = ctx.project_path.to_string_lossy().to_string();
    let timezone = schedule.timezone.as_deref().unwrap_or("UTC");
    let normalized_misfire = match schedule.misfire_policy.as_deref() {
        Some(p) if !p.is_empty() => p.to_string(),
        _ => match schedule.schedule_type.as_str() {
            "interval" => "fire_once_now".to_string(),
            _ => "skip".to_string(),
        },
    };
    let overlap_policy = schedule
        .overlap_policy
        .clone()
        .unwrap_or_else(|| "skip".to_string());
    let lateness_grace_secs = schedule.lateness_grace_secs.unwrap_or(60);
    let mut body = serde_json::json!({
        "spec_version": 1,
        "schedule_id": schedule.schedule_id,
        "item_ref": schedule.item_ref,
        "schedule_type": schedule.schedule_type,
        "expression": schedule.expression,
        "timezone": timezone,
        "enabled": schedule.enabled,
        "registered_at": registered_at,
        "misfire_policy": normalized_misfire,
        "overlap_policy": overlap_policy,
        "lateness_grace_secs": lateness_grace_secs,
        "project_root": canonical_project_path,
        "execution": {
            "requester_fingerprint": requester_fingerprint,
            "capabilities": capabilities,
        },
        "managed_by": {
            "type": MANAGED_BY_TYPE,
            "project_root": canonical_project_path,
            "project_key": ctx.project_key,
            "source_snapshot_hash": ctx.snapshot_hash,
            "source_path": desired.source_path,
            "source_body_hash": desired.source_body_hash,
        },
    });
    if !schedule.params.is_null() {
        body["params"] = schedule.params.clone();
    } else {
        body["params"] = serde_json::json!({});
    }

    let spec_path = writer::write_signed_node_item(
        node_dir,
        "schedules",
        &schedule.schedule_id,
        &body,
        &ctx.state.identity,
    )?;
    let content = fs::read_to_string(&spec_path)?;
    let signer_fingerprint =
        ryeos_scheduler::projection::parse_signer_fingerprint_from_str(&content)
            .unwrap_or_else(|| ctx.state.identity.fingerprint().to_string());
    let spec_hash = lillux::cas::sha256_hex(content.as_bytes());
    let rec = ScheduleSpecRecord {
        schedule_id: schedule.schedule_id.clone(),
        item_ref: schedule.item_ref.clone(),
        params: if schedule.params.is_null() {
            "{}".to_string()
        } else {
            serde_json::to_string(&schedule.params)?
        },
        schedule_type: schedule.schedule_type.clone(),
        expression: schedule.expression.clone(),
        timezone: timezone.to_string(),
        misfire_policy: normalized_misfire,
        overlap_policy,
        lateness_grace_secs,
        enabled: schedule.enabled,
        project_root: Some(canonical_project_path),
        signer_fingerprint,
        spec_hash,
        registered_at,
        requester_fingerprint: requester_fingerprint.to_string(),
        capabilities: capabilities.to_vec(),
    };
    ctx.state.scheduler_db.upsert_spec(&rec)?;
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
    schedules_dir: PathBuf,
    backups: Vec<ScheduleBackup>,
    touched: HashSet<String>,
}

impl ScheduleReconcileTx {
    fn new(schedules_dir: PathBuf) -> Self {
        Self {
            schedules_dir,
            backups: Vec::new(),
            touched: HashSet::new(),
        }
    }

    fn schedule_path(&self, schedule_id: &str) -> PathBuf {
        self.schedules_dir.join(format!("{schedule_id}.yaml"))
    }

    fn backup(&mut self, ctx: &ProjectDeployContext<'_>, schedule_id: &str) -> Result<()> {
        if self.backups.iter().any(|b| b.schedule_id == schedule_id) {
            return Ok(());
        }
        let yaml_path = self.schedule_path(schedule_id);
        let yaml_bytes = if yaml_path.exists() {
            Some(fs::read(&yaml_path).with_context(|| format!("backup {}", yaml_path.display()))?)
        } else {
            None
        };
        let db_record = ctx.state.scheduler_db.get_spec(schedule_id)?;
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

    fn finalize(self, ctx: &ProjectDeployContext<'_>) {
        reload_touched(ctx, &self.touched);
    }

    fn rollback(mut self, ctx: &ProjectDeployContext<'_>) {
        for backup in self.backups.iter().rev() {
            let yaml_path = self.schedule_path(&backup.schedule_id);
            match &backup.yaml_bytes {
                Some(bytes) => {
                    if let Some(parent) = yaml_path.parent() {
                        let _ = fs::create_dir_all(parent);
                    }
                    let _ = lillux::atomic_write(&yaml_path, bytes);
                }
                None => {
                    if yaml_path.exists() {
                        let _ = fs::remove_file(&yaml_path);
                    }
                }
            }

            match &backup.db_record {
                Some(record) => {
                    let _ = ctx.state.scheduler_db.upsert_spec(record);
                }
                None => {
                    let _ = ctx.state.scheduler_db.delete_spec(&backup.schedule_id);
                }
            }
            self.touched.insert(backup.schedule_id.clone());
        }
        reload_touched(ctx, &self.touched);
    }
}

fn reload_touched(ctx: &ProjectDeployContext<'_>, touched: &HashSet<String>) {
    if let Some(ref tx) = ctx.state.scheduler_reload_tx {
        for schedule_id in touched {
            if let Err(e) = tx.try_send(ryeos_scheduler::ReloadSignal {
                schedule_id: Some(schedule_id.clone()),
            }) {
                tracing::warn!(schedule_id = %schedule_id, error = %e, "scheduler reload channel full or closed — timer will pick up changes on next tick");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::require_project_reconcile_schedule_owner;
    use crate::handler_context::HandlerContext;
    use crate::handler_error::{extract_handler_error, HandlerError};

    fn verified(fp: &str) -> HandlerContext {
        HandlerContext::new(fp.to_string(), vec!["*".to_string()], true)
    }

    #[test]
    fn reconcile_owner_ok_for_owner() {
        let caller = verified("fp:owner");
        assert!(
            require_project_reconcile_schedule_owner(&caller, "snap-track-feed", "fp:owner")
                .is_ok()
        );
    }

    #[test]
    fn reconcile_owner_conflict_for_other_principal_without_leaking_owner() {
        let caller = verified("fp:caller2a88");
        let err = require_project_reconcile_schedule_owner(
            &caller,
            "snap-track-feed",
            "fp:owner_secret_fingerprint",
        )
        .expect_err("a schedule owned by another principal must conflict");

        // Typed as Conflict (→ 409 on both route and /execute), not NotFound.
        let he = extract_handler_error(&err).expect("typed HandlerError in chain");
        assert!(matches!(he, HandlerError::Conflict(_)), "got: {he:?}");

        let msg = format!("{err}");
        assert!(
            msg.contains("snap-track-feed"),
            "must name the schedule: {msg}"
        );
        assert!(
            msg.contains("different principal") && msg.contains("deregister"),
            "must be actionable: {msg}"
        );
        // Never disclose the conflicting owner's identity.
        assert!(
            !msg.contains("fp:owner_secret_fingerprint"),
            "must NOT leak the owner fingerprint: {msg}"
        );
    }

    #[test]
    fn reconcile_owner_fails_closed_as_notfound_for_unverified_caller() {
        // Defence in depth: an unverified caller never even learns of the
        // conflict — it gets the existence-hiding NotFound, as on direct access.
        let caller = HandlerContext::anonymous();
        let err = require_project_reconcile_schedule_owner(&caller, "snap-track-feed", "fp:owner")
            .expect_err("unverified caller must fail closed");
        let he = extract_handler_error(&err).expect("typed HandlerError in chain");
        assert!(matches!(he, HandlerError::NotFound), "got: {he:?}");
    }
}
