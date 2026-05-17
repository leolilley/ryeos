//! `scheduler.deregister` — remove a schedule spec.
//!
//! Ownership check: non-admin callers can only deregister their own schedules.

use std::sync::Arc;

use anyhow::Result;
use serde::Deserialize;
use serde_json::Value;

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

    // Delete YAML file
    let yaml_path = state.config.system_space_dir
        .join(ryeos_engine::AI_DIR).join("node").join("schedules")
        .join(format!("{}.yaml", req.schedule_id));
    if yaml_path.exists() {
        std::fs::remove_file(&yaml_path)
            .map_err(|e| HandlerError::Internal(e.to_string()))?;
    }

    // Delete projection rows
    state.scheduler_db.delete_spec(&req.schedule_id)
        .map_err(|e| HandlerError::Internal(e.to_string()))?;
    state.scheduler_db.delete_fires_for_schedule(&req.schedule_id)
        .map_err(|e| HandlerError::Internal(e.to_string()))?;

    // JSONL file on disk is preserved (audit trail)

    // Ping timer loop
    if let Some(ref tx) = state.scheduler_reload_tx {
        let _ = tx.try_send(ryeos_scheduler::ReloadSignal { schedule_id: Some(req.schedule_id.clone()) });
    }

    Ok(serde_json::json!({
        "schedule_id": req.schedule_id,
        "deleted": true,
        "history_preserved": true,
    }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:scheduler/deregister",
    endpoint: "scheduler.deregister",
    availability: ServiceAvailability::Both,
    required_caps: &["ryeos.execute.service.scheduler/deregister"],
    handler: |params, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, state).await.map_err(Into::into)
        })
    },
};
