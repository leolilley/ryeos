//! `scheduler.resume` — re-enable a paused schedule.
//!
//! Ownership check: callers resume their own schedules; node-owned
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
    let _mutation_guard = state.scheduler_runtime_gate.clone().write_owned().await;
    ryeos_scheduler::crontab::validate_schedule_id(&req.schedule_id)
        .map_err(|e| HandlerError::BadRequest(e.to_string()))?;
    let spec = state
        .scheduler_db
        .get_spec(&req.schedule_id)
        .map_err(|e| HandlerError::Internal(e.to_string()))?
        .ok_or(HandlerError::NotFound)?;

    // Same operator carve-out as scheduler_pause: node-owned schedules are
    // controllable by any verified local operator; everyone else controls
    // only their own.
    if spec.requester_fingerprint == state.identity.fingerprint() {
        ctx.require_verified()?;
    } else {
        ctx.require_owner(Some(&spec.requester_fingerprint))?;
    }

    if spec.enabled {
        super::scheduler_register::verify_schedule_enabled(&state, &spec, true)
            .map_err(|e| HandlerError::Internal(e.to_string()))?;
        return Ok(serde_json::json!({
            "schedule_id": req.schedule_id,
            "enabled": true,
        }));
    }

    let rec = super::scheduler_register::rewrite_schedule_enabled(&state, &spec, true)
        .map_err(|e| HandlerError::Internal(e.to_string()))?;
    state
        .scheduler_db
        .upsert_spec(&rec)
        .map_err(|e| HandlerError::Internal(e.to_string()))?;

    if let Some(ref tx) = state.scheduler_reload_tx {
        if let Err(e) = tx.try_send(ryeos_scheduler::ReloadSignal {
            schedule_id: Some(req.schedule_id.clone()),
        }) {
            tracing::warn!(schedule_id = %req.schedule_id, error = %e, "scheduler reload channel full or closed — timer will pick up changes on next tick");
        }
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
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, ctx, state).await.map_err(Into::into)
        })
    },
};
