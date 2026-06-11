//! `scheduler.pause` — disable a schedule without removing it.
//!
//! Ownership check: callers can only pause their own schedules.

use std::sync::Arc;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;

use crate::handler_error::HandlerError;
use crate::registry::ServiceDescriptor;
use ryeos_app::node_config::writer;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub schedule_id: String,
}

pub async fn handle(
    req: Request,
    ctx: crate::handler_context::HandlerContext,
    state: Arc<AppState>,
) -> Result<Value, HandlerError> {
    let _mutation_guard = state.scheduler_runtime_gate.clone().write_owned().await;
    ryeos_scheduler::crontab::validate_schedule_id(&req.schedule_id)
        .map_err(|e| HandlerError::BadRequest(e.to_string()))?;
    let spec = state
        .scheduler_db
        .get_spec(&req.schedule_id)
        .map_err(|e| HandlerError::Internal(e.to_string()))?
        .ok_or(HandlerError::NotFound)?;

    ctx.require_owner(Some(&spec.requester_fingerprint))?;

    if !spec.enabled {
        return Ok(serde_json::json!({
            "schedule_id": req.schedule_id,
            "enabled": false,
        }));
    }

    // Read current YAML, modify enabled, re-sign
    let node_dir = state
        .config
        .app_root
        .join(ryeos_engine::AI_DIR)
        .join("node");
    let yaml_path = node_dir
        .join("schedules")
        .join(format!("{}.yaml", req.schedule_id));

    let content = std::fs::read_to_string(&yaml_path)
        .with_context(|| format!("read schedule YAML {}", yaml_path.display()))
        .map_err(|e| HandlerError::Internal(e.to_string()))?;

    // Strip signature, parse, modify, re-serialize
    let body_str = lillux::signature::strip_signature_lines(&content);
    let mut body: serde_json::Value =
        serde_yaml::from_str(&body_str).map_err(|e| HandlerError::Internal(e.to_string()))?;
    body["enabled"] = Value::Bool(false);

    let spec_path = writer::write_signed_node_item(
        &node_dir,
        "schedules",
        &req.schedule_id,
        &body,
        &state.identity,
    )
    .map_err(|e| HandlerError::Internal(e.to_string()))?;

    // Update projection — preserve registered_at (immutable anchor)
    let content =
        std::fs::read_to_string(&spec_path).map_err(|e| HandlerError::Internal(e.to_string()))?;
    let mut rec = spec;
    rec.enabled = false;
    rec.signer_fingerprint =
        ryeos_scheduler::projection::parse_signer_fingerprint_from_str(&content)
            .unwrap_or_else(|| state.identity.fingerprint().to_string());
    rec.spec_hash = lillux::cas::sha256_hex(content.as_bytes());
    state
        .scheduler_db
        .upsert_spec(&rec)
        .map_err(|e| HandlerError::Internal(e.to_string()))?;

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
        "enabled": false,
    }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:scheduler/pause",
    endpoint: "scheduler.pause",
    availability: ServiceAvailability::Both,
    required_caps: &["ryeos.execute.service.scheduler/pause"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, ctx, state).await.map_err(Into::into)
        })
    },
};
