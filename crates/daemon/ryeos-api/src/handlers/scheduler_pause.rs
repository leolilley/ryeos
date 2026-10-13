//! `scheduler.pause` — disable a schedule without removing it.
//!
//! Ownership check: callers pause their own schedules; node-owned
//! schedules (daemon maintenance registrations) accept any verified
//! local operator.

use std::sync::Arc;

use anyhow::Result;
use serde::Deserialize;
use serde_json::Value;

use crate::handler_error::HandlerError;
use crate::registry::ServiceDescriptor;
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
    let scheduler = crate::api_state::selected_scheduler(state.as_ref())
        .map_err(|e| HandlerError::Internal(e.to_string()))?;
    let _mutation_guard = scheduler
        .acquire_mutation()
        .await
        .map_err(|e| HandlerError::Internal(e.to_string()))?;
    let scheduler_db = scheduler.scheduler_db();
    ryeos_scheduler_contract::validate_schedule_id(&req.schedule_id)
        .map_err(|e| HandlerError::BadRequest(e.to_string()))?;
    let spec = scheduler_db
        .get_spec(&req.schedule_id)
        .map_err(|e| HandlerError::Internal(e.to_string()))?
        .ok_or(HandlerError::NotFound)?;

    // Callers control their own schedules. NODE-owned schedules (the
    // daemon's maintenance registrations, requester = the node identity)
    // are controllable by any verified local operator: the operator can
    // invoke the scheduled service directly, so gating its cadence tighter
    // than the action itself would be incoherent.
    if spec.execution.principal_id() == scheduler.identity().principal_id() {
        ctx.require_verified()?;
    } else {
        ctx.require_owner(Some(spec.execution.principal_id()))?;
    }

    if !spec.enabled {
        super::scheduler_register::verify_schedule_enabled(&scheduler, &spec, false)
            .map_err(|e| HandlerError::Internal(e.to_string()))?;
        return Ok(serde_json::json!({
            "schedule_id": req.schedule_id,
            "enabled": false,
        }));
    }

    let rec = super::scheduler_register::rewrite_schedule_enabled(&scheduler, &spec, false)
        .map_err(|e| HandlerError::Internal(e.to_string()))?;
    scheduler_db
        .upsert_spec(&rec)
        .map_err(|e| HandlerError::Internal(e.to_string()))?;

    // Ping timer loop
    scheduler.notify_reload(Some(req.schedule_id.clone()));

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
