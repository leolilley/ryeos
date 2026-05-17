//! `scheduler.show_fires` — fire history for a schedule.
//!
//! Ownership check: non-admin callers can only view fires for
//! schedules they own.

use std::sync::Arc;

use anyhow::Result;
use serde::Deserialize;
use serde_json::Value;

use ryeos_executor::executor::ServiceAvailability;
use crate::registry::ServiceDescriptor;
use crate::handler_error::HandlerError;
use crate::handlers::ownership::require_owner_or_admin;
use ryeos_app::state::AppState;

fn default_limit() -> usize { 50 }

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub schedule_id: String,
    #[serde(default = "default_limit")]
    pub limit: usize,
    #[serde(default)]
    pub status: Option<String>,
    /// Injected by service_invocation for ownership checks.
    #[serde(default)]
    pub _caller_fingerprint: String,
    /// Injected by service_invocation for ownership checks.
    #[serde(default)]
    pub _caller_scopes: Vec<String>,
}

pub async fn handle(req: Request, state: Arc<AppState>) -> Result<Value, HandlerError> {
    let spec = state.scheduler_db.get_spec(&req.schedule_id)
        .map_err(|e| HandlerError::Internal(e.to_string()))?
        .ok_or(HandlerError::NotFound)?;

    require_owner_or_admin(
        Some(spec.requester_fingerprint.as_str()),
        &req._caller_fingerprint,
        &req._caller_scopes,
    )?;

    let limit = req.limit.clamp(1, 500);
    let (fires, total) = state.scheduler_db.list_fires(
        &req.schedule_id,
        req.status.as_deref(),
        limit,
    ).map_err(|e| HandlerError::Internal(e.to_string()))?;

    let fire_entries: Vec<Value> = fires.iter().map(|f| {
        serde_json::json!({
            "fire_id": f.fire_id,
            "schedule_id": f.schedule_id,
            "scheduled_at": f.scheduled_at,
            "fired_at": f.fired_at,
            "thread_id": f.thread_id,
            "status": f.status,
            "trigger_reason": f.trigger_reason,
            "outcome": f.outcome,
            "signer_fingerprint": f.signer_fingerprint,
        })
    }).collect();

    Ok(serde_json::json!({
        "schedule_id": req.schedule_id,
        "fires": fire_entries,
        "total": total,
        "returned": fire_entries.len(),
    }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:scheduler/show_fires",
    endpoint: "scheduler.show_fires",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, state).await.map_err(Into::into)
        })
    },
};
