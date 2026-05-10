//! `scheduler.register` — create or update a schedule spec.

use std::sync::Arc;

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::Value;

use crate::node_config::writer;
use crate::scheduler::crontab;
use crate::scheduler::projection;
use crate::scheduler::types::ScheduleSpecRecord;
use crate::service_executor::ServiceAvailability;
use crate::service_registry::ServiceDescriptor;
use crate::state::AppState;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub schedule_id: String,
    pub item_ref: String,
    pub schedule_type: String,
    pub expression: String,
    #[serde(default)]
    pub params: Value,
    #[serde(default)]
    pub timezone: Option<String>,
    #[serde(default)]
    pub misfire_policy: Option<String>,
    #[serde(default)]
    pub overlap_policy: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub project_root: Option<String>,
    /// Injected by execute_service_verified from ExecutionContext.
    /// The caller's fingerprint — used as the schedule's acting principal.
    #[serde(default, rename = "_caller_fingerprint")]
    pub caller_fingerprint: Option<String>,
    /// Injected by execute_service_verified from ExecutionContext.
    /// The caller's capabilities — the schedule runs with only these.
    #[serde(default, rename = "_caller_capabilities")]
    pub caller_capabilities: Option<Vec<String>>,
}

fn default_true() -> bool { true }

pub async fn handle(req: Request, state: Arc<AppState>) -> Result<Value> {
    // Validate
    crontab::validate_schedule_id(&req.schedule_id)?;
    ryeos_engine::canonical_ref::CanonicalRef::parse(&req.item_ref)
        .with_context(|| format!("invalid item_ref: {}", req.item_ref))?;
    crontab::validate_expression(&req.schedule_type, &req.expression)?;

    let timezone = req.timezone.as_deref().unwrap_or("UTC");
    crontab::validate_timezone(timezone)?;

    // All overlap policies ship day 1: allow, skip, cancel_previous.
    if let Some(ref p) = req.overlap_policy {
        if !matches!(p.as_str(), "allow" | "skip" | "cancel_previous") {
            bail!("invalid overlap_policy: {}", p);
        }
    }
    // All misfire policies ship day 1: skip, fire_once_now, catch_up_bounded:N, catch_up_within_secs:S.
    if let Some(ref p) = req.misfire_policy {
        if !is_valid_misfire_policy(p) {
            bail!("invalid misfire_policy: {}", p);
        }
    }

    if req.schedule_type == "at" && crontab::is_at_past(&req.expression, lillux::time::timestamp_millis()) {
        bail!("at schedule timestamp is in the past");
    }

    // Check if schedule already exists (for preserving registered_at on update)
    let existing_spec = state.scheduler_db.get_spec(&req.schedule_id)?;

    // Disallow schedule_id reuse if fire history exists from a previous schedule.
    // Prevents old JSONL from corrupting new schedule on rebuild.
    // existing_spec.is_none() means this is a new registration, not an update.
    let fires_dir = state.config.system_space_dir
        .join(ryeos_engine::AI_DIR).join("state").join("schedules")
        .join(&req.schedule_id);
    if fires_dir.exists() && existing_spec.is_none() {
        bail!(
            "schedule_id '{}' reuse not allowed: fire history exists at {} — deregister first or use a different ID",
            req.schedule_id,
            fires_dir.display()
        );
    }

    // Build YAML body — preserve registered_at on updates for deterministic first-fire.
    // The YAML body is the canonical source of truth for registered_at.
    // We read it from the existing file (not DB) so repeated updates don't drift the anchor.
    let registered_at = if existing_spec.is_some() {
        let existing_yaml_path = state.config.system_space_dir
            .join(ryeos_engine::AI_DIR).join("node").join("schedules")
            .join(&req.schedule_id)
            .with_extension("yaml");
        std::fs::read_to_string(&existing_yaml_path)
            .ok()
            .and_then(|content| {
                let body_str = lillux::signature::strip_signature_lines(&content);
                let body: serde_json::Value = serde_yaml::from_str(&body_str).ok()?;
                body.get("registered_at").and_then(|v| v.as_i64())
            })
            .unwrap_or_else(|| existing_spec.as_ref().map(|s| s.registered_at).unwrap_or_else(lillux::time::timestamp_millis))
    } else {
        lillux::time::timestamp_millis()
    };
    let mut body = serde_json::json!({
        "spec_version": 1,
        "section": "schedules",
        "schedule_id": req.schedule_id,
        "item_ref": req.item_ref,
        "schedule_type": req.schedule_type,
        "expression": req.expression,
        "timezone": timezone,
        "enabled": req.enabled,
        "registered_at": registered_at,
    });
    if !req.params.is_null() {
        body["params"] = req.params.clone();
    }
    // Normalize misfire_policy: resolve default so YAML and DB always have
    // the same resolved value (no empty strings that behave differently
    // in projection vs live paths).
    let normalized_misfire = match req.misfire_policy.as_deref() {
        Some(p) if !p.is_empty() => p.to_string(),
        _ => match req.schedule_type.as_str() {
            "interval" => "fire_once_now".to_string(),
            _ => "skip".to_string(),
        },
    };
    body["misfire_policy"] = Value::String(normalized_misfire.clone());
    if let Some(ref p) = req.overlap_policy {
        body["overlap_policy"] = Value::String(p.clone());
    }
    // Determine execution authority from caller context.
    // The service executor injects these from ExecutionContext before
    // dispatching the handler. Fail-closed: if injection didn't happen,
    // error out rather than silently degrading to node identity.
    let requester_fingerprint = req.caller_fingerprint.clone()
        .ok_or_else(|| anyhow::anyhow!(
            "scheduler.register requires verified caller context \
             (executor must inject _caller_fingerprint)"
        ))?;
    let capabilities = req.caller_capabilities.clone()
        .filter(|caps| !caps.is_empty())
        .ok_or_else(|| anyhow::anyhow!(
            "scheduler.register requires verified caller context \
             with non-empty _caller_capabilities"
        ))?;

    if let Some(ref p) = req.project_root {
        body["project_root"] = Value::String(p.clone());
    }

    // Persist execution authority in YAML body — survives restart, rebuildable.
    body["execution"] = serde_json::json!({
        "requester_fingerprint": requester_fingerprint,
        "capabilities": capabilities,
    });

    // Write signed YAML
    let node_dir = state.config.system_space_dir.join(ryeos_engine::AI_DIR).join("node");
    let spec_path = writer::write_signed_node_item(
        &node_dir,
        "schedules",
        &req.schedule_id,
        &body,
        &state.identity,
    )?;

    // Extract signer fingerprint
    let content = std::fs::read_to_string(&spec_path)?;
    let signer_fingerprint = projection::parse_signer_fingerprint_from_str(&content)
        .unwrap_or_else(|| state.identity.fingerprint().to_string());

    // Compute hash
    let spec_hash = lillux::cas::sha256_hex(content.as_bytes());
    // Use registered_at as the scheduling anchor — immutable across updates.
    // This ensures the timer always fires at the same intervals regardless of
    // when the schedule was last modified.
    let registered_at_db = registered_at;

    // Upsert projection
    let was_existing = existing_spec.is_some();
    let overlap_policy = req.overlap_policy.unwrap_or_else(|| "skip".to_string());
    let rec = ScheduleSpecRecord {
        schedule_id: req.schedule_id.clone(),
        item_ref: req.item_ref.clone(),
        params: serde_json::to_string(&req.params)? ,
        schedule_type: req.schedule_type.clone(),
        expression: req.expression.clone(),
        timezone: timezone.to_string(),
        misfire_policy: normalized_misfire.clone(),
        overlap_policy: overlap_policy.clone(),
        enabled: req.enabled,
        project_root: req.project_root.clone(),
        signer_fingerprint,
        spec_hash,
        registered_at: registered_at_db,
        requester_fingerprint,
        capabilities,
    };
    state.scheduler_db.upsert_spec(&rec)?;

    // Ping timer loop
    if let Some(ref tx) = state.scheduler_reload_tx {
        let _ = tx.try_send(crate::scheduler::ReloadSignal { schedule_id: Some(req.schedule_id.clone()) });
    }

    Ok(serde_json::json!({
        "schedule_id": req.schedule_id,
        "item_ref": req.item_ref,
        "schedule_type": req.schedule_type,
        "expression": req.expression,
        "timezone": timezone,
        "misfire_policy": normalized_misfire,
        "overlap_policy": overlap_policy,
        "enabled": req.enabled,
        "spec_path": spec_path.display().to_string(),
        "created": !was_existing,
    }))
}

fn is_valid_misfire_policy(p: &str) -> bool {
    match p {
        "skip" | "fire_once_now" => true,
        s if s.starts_with("catch_up_bounded:") => {
            s.strip_prefix("catch_up_bounded:")
                .and_then(|n| n.parse::<usize>().ok())
                .is_some()
        }
        s if s.starts_with("catch_up_within_secs:") => {
            s.strip_prefix("catch_up_within_secs:")
                .and_then(|n| n.parse::<u64>().ok())
                .is_some()
        }
        _ => false,
    }
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:scheduler/register",
    endpoint: "scheduler.register",
    availability: ServiceAvailability::Both,
    required_caps: &["ryeos.execute.service.scheduler/register"],
    handler: |params, state| {
        Box::pin(async move {
            let req: Request = serde_json::from_value(params)?;
            handle(req, state).await
        })
    },
};
