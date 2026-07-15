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
    let _mutation_guard = state.scheduler_runtime_gate.clone().write_owned().await;
    ryeos_scheduler::crontab::validate_schedule_id(&req.schedule_id)
        .map_err(|e| HandlerError::BadRequest(e.to_string()))?;
    let spec = state
        .scheduler_db
        .get_spec(&req.schedule_id)
        .map_err(|e| HandlerError::Internal(e.to_string()))?
        .ok_or(HandlerError::NotFound)?;

    // Callers control their own schedules. NODE-owned schedules (the
    // daemon's maintenance registrations, requester = the node identity)
    // are controllable by any verified local operator: the operator can
    // invoke the scheduled service directly, so gating its cadence tighter
    // than the action itself would be incoherent.
    if spec.requester_fingerprint == state.identity.fingerprint() {
        ctx.require_verified()?;
    } else {
        ctx.require_owner(Some(&spec.requester_fingerprint))?;
    }

    if !spec.enabled {
        super::scheduler_register::verify_schedule_enabled(&state, &spec, false)
            .map_err(|e| HandlerError::Internal(e.to_string()))?;
        return Ok(serde_json::json!({
            "schedule_id": req.schedule_id,
            "enabled": false,
        }));
    }

    let rec = super::scheduler_register::rewrite_schedule_enabled(&state, &spec, false)
        .map_err(|e| HandlerError::Internal(e.to_string()))?;
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
