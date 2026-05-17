//! `scheduler.resume` — re-enable a paused schedule.
//!
//! Ownership check: non-admin callers can only resume their own schedules.

use std::sync::Arc;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;

use ryeos_app::node_config::writer;
use ryeos_executor::executor::ServiceAvailability;
use crate::registry::ServiceDescriptor;
use crate::handler_error::HandlerError;
use crate::handlers::ownership::require_owner_or_admin;
use ryeos_app::state::AppState;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub schedule_id: String,
    /// Injected by service_invocation for ownership checks.
    #[serde(default)]
    pub _caller_fingerprint: String,
    /// Injected by service_invocation for ownership checks.
    #[serde(default)]
    pub _caller_scopes: Vec<String>,
}

pub async fn handle(req: Request, state: Arc<AppState>) -> Result<Value, HandlerError> {
    ryeos_scheduler::crontab::validate_schedule_id(&req.schedule_id)
        .map_err(|e| HandlerError::BadRequest(e.to_string()))?;
    let spec = state.scheduler_db.get_spec(&req.schedule_id)
        .map_err(|e| HandlerError::Internal(e.to_string()))?
        .ok_or(HandlerError::NotFound)?;

    require_owner_or_admin(
        Some(&spec.requester_fingerprint),
        &req._caller_fingerprint,
        &req._caller_scopes,
    )?;

    if spec.enabled {
        return Ok(serde_json::json!({
            "schedule_id": req.schedule_id,
            "enabled": true,
        }));
    }

    let node_dir = state.config.system_space_dir.join(ryeos_engine::AI_DIR).join("node");
    let yaml_path = node_dir
        .join("schedules")
        .join(format!("{}.yaml", req.schedule_id));

    let content = std::fs::read_to_string(&yaml_path)
        .with_context(|| format!("read schedule YAML {}", yaml_path.display()))
        .map_err(|e| HandlerError::Internal(e.to_string()))?;

    let body_str = lillux::signature::strip_signature_lines(&content);
    let mut body: serde_json::Value = serde_yaml::from_str(&body_str)
        .map_err(|e| HandlerError::Internal(e.to_string()))?;
    body["enabled"] = Value::Bool(true);

    writer::write_signed_node_item(
        &node_dir,
        "schedules",
        &req.schedule_id,
        &body,
        &state.identity,
    ).map_err(|e| HandlerError::Internal(e.to_string()))?;

    // Update projection — preserve registered_at (immutable anchor)
    let mut rec = spec;
    rec.enabled = true;
    state.scheduler_db.upsert_spec(&rec)
        .map_err(|e| HandlerError::Internal(e.to_string()))?;

    if let Some(ref tx) = state.scheduler_reload_tx {
        let _ = tx.try_send(ryeos_scheduler::ReloadSignal { schedule_id: Some(req.schedule_id.clone()) });
    }

    Ok(serde_json::json!({
        "schedule_id": req.schedule_id,
        "enabled": true,
    }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:scheduler/resume",
    endpoint: "scheduler.resume",
    availability: ServiceAvailability::Both,
    required_caps: &["ryeos.execute.service.scheduler/resume"],
    handler: |params, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, state).await.map_err(Into::into)
        })
    },
};
