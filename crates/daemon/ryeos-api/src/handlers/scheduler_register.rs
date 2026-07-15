//! `scheduler.register` — create or update a schedule spec.

use std::collections::BTreeMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::Value;

use crate::registry::ServiceDescriptor;
use ryeos_app::node_config::writer;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;
use ryeos_scheduler::crontab;
use ryeos_scheduler::projection;
use ryeos_scheduler::types::{ScheduleExecution, ScheduleManagedBy, ScheduleSourceRecord};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub schedule_id: String,
    pub item_ref: String,
    pub ref_bindings: BTreeMap<String, String>,
    pub schedule_type: String,
    pub expression: String,
    pub params: Value,
    pub timezone: String,
    pub misfire_policy: String,
    pub overlap_policy: String,
    pub lateness_grace_secs: i64,
    pub enabled: bool,
    #[serde(default)]
    pub project_root: Option<String>,
}

pub async fn handle(
    req: Request,
    ctx: crate::handler_context::HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    // Caller identity is used for execution authority — must be verified.
    ctx.require_verified().map_err(|e| anyhow::anyhow!(e))?;
    let _mutation_guard = state.scheduler_runtime_gate.clone().write_owned().await;

    // Validate
    crontab::validate_schedule_id(&req.schedule_id)?;
    ryeos_engine::canonical_ref::CanonicalRef::parse(&req.item_ref)
        .with_context(|| format!("invalid item_ref: {}", req.item_ref))?;
    ryeos_executor::execution::launch_preparation::validate_ref_bindings(&req.ref_bindings)?;
    crontab::validate_expression(&req.schedule_type, &req.expression)?;

    crontab::validate_timezone(&req.timezone)?;

    // All overlap policies ship day 1: allow, skip, cancel_previous.
    ryeos_scheduler::overlap::parse_overlap_policy(&req.overlap_policy)?;
    // All misfire policies ship day 1: skip, fire_once_now, catch_up_bounded:N, catch_up_within_secs:S.
    ryeos_scheduler::misfire::parse_misfire_policy(&req.misfire_policy)?;
    if req.lateness_grace_secs <= 0 {
        bail!(
            "lateness_grace_secs must be positive, got: {}",
            req.lateness_grace_secs
        );
    }
    if !req.params.is_object() {
        bail!("params must be a JSON object");
    }
    if req
        .project_root
        .as_deref()
        .is_some_and(|project_root| project_root.trim().is_empty())
    {
        bail!("project_root must be non-empty when present");
    }

    if req.schedule_type == "at"
        && crontab::is_at_past(&req.expression, lillux::time::timestamp_millis())
    {
        bail!("at schedule timestamp is in the past");
    }

    // Check if schedule already exists (for preserving registered_at on update)
    let existing_spec = state.scheduler_db.get_spec(&req.schedule_id)?;

    // Ownership check on update: the caller must be the existing
    // requester. On create, no ownership check needed —
    // the caller becomes the owner.
    if let Some(ref existing) = existing_spec {
        ctx.require_owner(Some(&existing.requester_fingerprint))
            .map_err(|e| -> anyhow::Error { e.into() })?;
    }

    // Disallow schedule_id reuse if fire history exists from a previous schedule.
    // Prevents old JSONL from corrupting new schedule on rebuild.
    // existing_spec.is_none() means this is a new registration, not an update.
    let fires_dir = state
        .config
        .app_root
        .join(ryeos_engine::AI_DIR)
        .join("state")
        .join("schedules")
        .join(&req.schedule_id);
    if existing_spec.is_none() {
        match std::fs::symlink_metadata(&fires_dir) {
            Ok(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {
                bail!(
                    "schedule_id '{}' reuse not allowed: fire history exists at {} — \
                         deregister with purge_history=true to clear it (scheduler/deregister), \
                         or use a different ID",
                    req.schedule_id,
                    fires_dir.display()
                );
            }
            Ok(_) => bail!(
                "schedule history path must be a real directory: {}",
                fires_dir.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }

    let source_mutation = ScheduleSourceMutation::acquire(
        state.as_ref(),
        &req.schedule_id,
        existing_spec.as_ref().map(|spec| spec.spec_hash.as_str()),
    )?;
    let existing_source = source_mutation.verified.as_ref();
    if existing_spec.is_none() && existing_source.is_some() {
        bail!(
            "schedule_id '{}' has signed node YAML but no scheduler projection; rebuild the projection or remove the node item before registering",
            req.schedule_id
        );
    }
    let existing_record = existing_source.map(|source| &source.record);
    if existing_record.is_some_and(is_project_managed_schedule) {
        bail!(
            "schedule_id '{}' is project-managed; update it through project sync or deregister first",
            req.schedule_id
        );
    }

    // Build YAML body — preserve registered_at on updates for deterministic first-fire.
    // The YAML body is the canonical source of truth for registered_at.
    // We read it from the existing file (not DB) so repeated updates don't drift the anchor.
    let registered_at = if existing_spec.is_some() {
        existing_record
            .context("existing schedule projection has no signed source file")?
            .registered_at
    } else {
        lillux::time::timestamp_millis()
    };
    // Determine execution authority from caller context.
    // The service executor injects _ctx from ExecutionContext before
    // dispatching the handler. Fail-closed: if injection didn't happen,
    // error out rather than silently degrading to node identity.
    let caller_fingerprint = if ctx.fingerprint.is_empty() {
        bail!("scheduler.register requires verified caller context (executor must inject _ctx)");
    } else {
        ctx.fingerprint.clone()
    };
    let capabilities = if let Some(ref existing) = existing_spec {
        existing.capabilities.clone()
    } else if ctx.scopes.is_empty() {
        bail!("scheduler.register requires verified caller context with non-empty scopes");
    } else {
        ctx.scopes.clone()
    };

    // On UPDATE, preserve the existing requester_fingerprint — only the
    // original owner can update, but the owner identity and
    // granted capabilities stay the same. On CREATE, the caller becomes
    // the owner and current caller scopes become the schedule grant.
    let requester_fingerprint = if let Some(ref existing) = existing_spec {
        existing.requester_fingerprint.clone()
    } else {
        caller_fingerprint
    };

    let source_record = ScheduleSourceRecord {
        spec_version: 1,
        schedule_id: req.schedule_id.clone(),
        item_ref: req.item_ref.clone(),
        ref_bindings: req.ref_bindings.clone(),
        schedule_type: req.schedule_type.clone(),
        expression: req.expression.clone(),
        params: req.params.clone(),
        timezone: req.timezone.clone(),
        misfire_policy: req.misfire_policy.clone(),
        overlap_policy: req.overlap_policy.clone(),
        lateness_grace_secs: req.lateness_grace_secs,
        enabled: req.enabled,
        project_root: req.project_root.clone(),
        registered_at,
        execution: ScheduleExecution {
            requester_fingerprint,
            capabilities,
        },
        managed_by: None,
    };
    source_record.validate(Some(&req.schedule_id))?;
    let body = serde_json::to_value(&source_record)?;

    // Write signed YAML through the same pinned source transaction used for
    // the verified existing-source read.
    let (spec_path, verified) = source_mutation.publish(&body, &state.identity)?;
    let rec = verified.to_spec_record()?;
    let was_existing = existing_spec.is_some();
    state.scheduler_db.upsert_spec(&rec)?;

    // Ping timer loop
    if let Some(ref tx) = state.scheduler_reload_tx {
        if let Err(e) = tx.try_send(ryeos_scheduler::ReloadSignal {
            schedule_id: Some(req.schedule_id.clone()),
        }) {
            tracing::warn!(schedule_id = %req.schedule_id, error = %e, "scheduler reload channel full or closed — timer will pick up changes on next tick");
        }
    }

    Ok(serde_json::json!({
        "schedule_id": req.schedule_id,
        "item_ref": req.item_ref,
        "ref_bindings": req.ref_bindings,
        "schedule_type": req.schedule_type,
        "expression": req.expression,
        "timezone": req.timezone,
        "misfire_policy": req.misfire_policy,
        "overlap_policy": req.overlap_policy,
        "lateness_grace_secs": req.lateness_grace_secs,
        "enabled": req.enabled,
        "spec_path": spec_path.display().to_string(),
        "created": !was_existing,
    }))
}

pub(super) fn canonical_schedule_source_path(
    app_root: &Path,
    schedule_id: &str,
) -> Result<PathBuf> {
    crontab::validate_schedule_id(schedule_id)?;
    let schedules_dir = app_root
        .join(ryeos_engine::AI_DIR)
        .join("node")
        .join("schedules");
    let _ = projection::canonical_schedule_source_paths(&schedules_dir)?;
    Ok(schedules_dir.join(format!("{schedule_id}.yaml")))
}

pub(super) fn load_existing_schedule_source(
    state: &AppState,
    schedule_id: &str,
    expected_spec_hash: Option<&str>,
) -> Result<Option<projection::VerifiedScheduleSource>> {
    let source = canonical_schedule_source_path(&state.config.app_root, schedule_id)?;
    match std::fs::symlink_metadata(&source) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("inspect schedule source {}", source.display()));
        }
    }
    let verified = projection::load_verified_schedule_source(&source, &state.engine.trust_store)?;
    if let Some(expected) = expected_spec_hash {
        if verified.spec_hash != expected {
            bail!(
                "schedule_id '{}' node source changed outside its scheduler projection; rebuild before updating (expected {}, got {})",
                schedule_id,
                expected,
                verified.spec_hash,
            );
        }
    }
    Ok(Some(verified))
}

/// One directory-locked, inode-pinned schedule mutation. The verified source
/// and conditional replacement are bound to the same directory descriptor and
/// exact file handle, so a path swap cannot redirect an authorized update.
struct ScheduleSourceMutation<'a> {
    directory: lillux::PinnedDirectory,
    _directory_lock: lillux::PinnedDirectoryLock,
    path: PathBuf,
    name: std::ffi::OsString,
    current_file: Option<std::fs::File>,
    verified: Option<projection::VerifiedScheduleSource>,
    trust_store: &'a ryeos_engine::trust::TrustStore,
}

impl<'a> ScheduleSourceMutation<'a> {
    fn acquire(
        state: &'a AppState,
        schedule_id: &str,
        expected_spec_hash: Option<&str>,
    ) -> Result<Self> {
        crontab::validate_schedule_id(schedule_id)?;
        let node_path = state
            .config
            .app_root
            .join(ryeos_engine::AI_DIR)
            .join("node");
        let node_directory = lillux::PinnedDirectory::open_or_create(&node_path)?;
        let directory =
            node_directory.open_or_create_child(std::ffi::OsStr::new("schedules"), 0o777)?;
        let directory_lock = directory.lock_exclusive()?;
        let name = std::ffi::OsString::from(format!("{schedule_id}.yaml"));
        let path = directory.path().join(&name);
        let current_file = directory.open_regular(&name, false)?;
        let verified = if let Some(file) = current_file.as_ref() {
            let mut content = String::new();
            file.try_clone()?.read_to_string(&mut content)?;
            Some(projection::verify_schedule_source_content(
                &path,
                &content,
                &state.engine.trust_store,
            )?)
        } else {
            None
        };
        if let (Some(expected), Some(verified)) = (expected_spec_hash, verified.as_ref()) {
            if verified.spec_hash != expected {
                bail!(
                    "schedule_id '{}' node source changed outside its scheduler projection; rebuild before updating (expected {}, got {})",
                    schedule_id,
                    expected,
                    verified.spec_hash,
                );
            }
        } else if expected_spec_hash.is_some() && verified.is_none() {
            bail!("existing schedule projection has no signed source file");
        }
        Ok(Self {
            directory,
            _directory_lock: directory_lock,
            path,
            name,
            current_file,
            verified,
            trust_store: &state.engine.trust_store,
        })
    }

    fn publish(
        &self,
        body: &Value,
        identity: &ryeos_app::identity::NodeIdentity,
    ) -> Result<(PathBuf, projection::VerifiedScheduleSource)> {
        let schedule_id = self
            .path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .context("schedule source filename must be UTF-8")?;
        let bytes = writer::render_signed_node_item("schedules", schedule_id, body, identity)?;
        self.directory.atomic_write_if_same(
            &self.name,
            self.current_file.as_ref(),
            &bytes,
            0o600,
        )?;
        let content = String::from_utf8(bytes).context("signed schedule source is not UTF-8")?;
        let verified =
            projection::verify_schedule_source_content(&self.path, &content, self.trust_store)?;
        Ok((self.path.clone(), verified))
    }
}

fn is_project_managed_schedule(record: &ScheduleSourceRecord) -> bool {
    matches!(
        &record.managed_by,
        Some(ScheduleManagedBy::ProjectAiSync { .. })
    )
}

pub(super) fn verify_schedule_enabled(
    state: &AppState,
    current: &ryeos_scheduler::types::ScheduleSpecRecord,
    enabled: bool,
) -> Result<()> {
    let source =
        load_existing_schedule_source(state, &current.schedule_id, Some(&current.spec_hash))?
            .with_context(|| {
                format!(
                    "schedule projection {} has no trusted source",
                    current.schedule_id
                )
            })?;
    if source.record.enabled != enabled {
        bail!(
            "schedule {} source enabled state diverges from its projection",
            current.schedule_id
        );
    }
    Ok(())
}

pub(super) fn rewrite_schedule_enabled(
    state: &AppState,
    current: &ryeos_scheduler::types::ScheduleSpecRecord,
    enabled: bool,
) -> Result<ryeos_scheduler::types::ScheduleSpecRecord> {
    let mutation =
        ScheduleSourceMutation::acquire(state, &current.schedule_id, Some(&current.spec_hash))?;
    let source = mutation.verified.as_ref().with_context(|| {
        format!(
            "schedule projection {} has no trusted source",
            current.schedule_id
        )
    })?;
    let mut record = source.record.clone();
    record.enabled = enabled;
    record.validate(Some(&current.schedule_id))?;

    let body = serde_json::to_value(&record)?;
    let (_path, verified) = mutation.publish(&body, &state.identity)?;
    if verified.record.enabled != enabled {
        bail!(
            "rewritten schedule {} did not retain requested enabled state",
            current.schedule_id
        );
    }
    verified.to_spec_record()
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:scheduler/register",
    endpoint: "scheduler.register",
    availability: ServiceAvailability::Both,
    required_caps: &["ryeos.execute.service.scheduler/register"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, ctx, state).await
        })
    },
};
