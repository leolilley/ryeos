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

    if let Some(ref p) = req.overlap_policy {
        if !matches!(p.as_str(), "allow" | "skip" | "cancel_previous") {
            bail!("invalid overlap_policy: {}", p);
        }
    }
    if let Some(ref p) = req.misfire_policy {
        if !is_valid_misfire_policy(p) {
            bail!("invalid misfire_policy: {}", p);
        }
    }

    if req.schedule_type == "at" && crontab::is_at_past(&req.expression, lillux::time::timestamp_millis()) {
        bail!("at schedule timestamp is in the past");
    }

    // Build YAML body
    let mut body = serde_json::json!({
        "spec_version": 1,
        "section": "schedules",
        "schedule_id": req.schedule_id,
        "item_ref": req.item_ref,
        "schedule_type": req.schedule_type,
        "expression": req.expression,
        "timezone": timezone,
        "enabled": req.enabled,
    });
    if !req.params.is_null() {
        body["params"] = req.params.clone();
    }
    if let Some(ref p) = req.misfire_policy {
        body["misfire_policy"] = Value::String(p.clone());
    }
    if let Some(ref p) = req.overlap_policy {
        body["overlap_policy"] = Value::String(p.clone());
    }
    if let Some(ref p) = req.project_root {
        body["project_root"] = Value::String(p.clone());
    }

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
    let last_modified = lillux::time::timestamp_millis();

    // Upsert projection
    let was_existing = state.scheduler_db.get_spec(&req.schedule_id)?.is_some();
    let misfire_policy = req.misfire_policy.unwrap_or_default();
    let overlap_policy = req.overlap_policy.unwrap_or_else(|| "skip".to_string());
    let rec = ScheduleSpecRecord {
        schedule_id: req.schedule_id.clone(),
        item_ref: req.item_ref.clone(),
        params: serde_json::to_string(&req.params)? ,
        schedule_type: req.schedule_type.clone(),
        expression: req.expression.clone(),
        timezone: timezone.to_string(),
        misfire_policy: misfire_policy.clone(),
        overlap_policy: overlap_policy.clone(),
        enabled: req.enabled,
        project_root: req.project_root.clone(),
        signer_fingerprint,
        spec_hash,
        last_modified,
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
        "misfire_policy": misfire_policy,
        "overlap_policy": overlap_policy,
        "enabled": req.enabled,
        "spec_path": spec_path.display().to_string(),
        "created": !was_existing,
    }))
}

fn is_valid_misfire_policy(p: &str) -> bool {
    matches!(p, "skip" | "fire_once_now")
        || p.starts_with("catch_up_bounded:")
        || p.starts_with("catch_up_within_secs:")
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:scheduler/register",
    endpoint: "scheduler.register",
    availability: ServiceAvailability::Both,
    required_caps: &[],
    handler: |params, state| {
        Box::pin(async move {
            let req: Request = serde_json::from_value(params)?;
            handle(req, state).await
        })
    },
};
