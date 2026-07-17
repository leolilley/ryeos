//! `scheduler.explain` — explain a schedule's current planning state.
//!
//! Ownership check: callers can only explain schedules they own.

use std::sync::Arc;

use anyhow::Result;
use serde::Deserialize;
use serde_json::Value;

use crate::handler_error::HandlerError;
use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;
use ryeos_scheduler::planning;

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
    ryeos_scheduler::crontab::validate_schedule_id(&req.schedule_id)
        .map_err(|e| HandlerError::BadRequest(e.to_string()))?;

    let spec = state
        .scheduler_db
        .get_spec(&req.schedule_id)
        .map_err(|e| HandlerError::Internal(e.to_string()))?
        .ok_or(HandlerError::NotFound)?;

    ctx.require_owner(Some(spec.requester_fingerprint.as_str()))?;

    let last_fire = state
        .scheduler_db
        .get_last_fire(&req.schedule_id)
        .map_err(|e| HandlerError::Internal(e.to_string()))?;
    let total_fires = state
        .scheduler_db
        .count_fires(&req.schedule_id, None)
        .map_err(|e| HandlerError::Internal(e.to_string()))?;
    let cursor = state
        .scheduler_db
        .get_cursor(&req.schedule_id)
        .map_err(|e| HandlerError::Internal(e.to_string()))?
        .filter(|cursor| cursor.spec_hash == spec.spec_hash);
    let now = lillux::time::timestamp_millis();
    let plan = planning::plan_schedule(&spec, last_fire.as_ref(), now);

    let last_fire_json = last_fire.as_ref().map(|f| {
        serde_json::json!({
            "fire_id": f.fire_id,
            "scheduled_at": f.scheduled_at,
            "fired_at": f.fired_at,
            "completed_at": f.completed_at,
            "thread_id": f.thread_id,
            "status": f.status,
            "trigger_reason": f.trigger_reason,
            "outcome": f.outcome,
            "signer_fingerprint": f.signer_fingerprint,
        })
    });

    Ok(serde_json::json!({
        "schedule_id": spec.schedule_id,
        "item_ref": spec.item_ref,
        "ref_bindings": spec.ref_bindings,
        "schedule_type": spec.schedule_type,
        "expression": spec.expression,
        "timezone": spec.timezone,
        "enabled": spec.enabled,
        "registered_at": spec.registered_at,
        "misfire_policy": spec.misfire_policy,
        "overlap_policy": spec.overlap_policy,
        "lateness_grace_secs": spec.lateness_grace_secs,
        "signer_fingerprint": spec.signer_fingerprint,
        "requester_fingerprint": spec.requester_fingerprint,
        "now": now,
        "last_fire": last_fire_json,
        "last_scheduled_at": plan.last_scheduled_at,
        "last_fire_at": plan.last_fire_at,
        "current_due_at": plan.current_due_at,
        "current_due_within_grace": plan.current_due_within_grace,
        "misfire_horizon_exclusive": plan.misfire_horizon_exclusive,
        "next_fire_at": plan.next_fire_at,
        "cursor_last_scheduled_at": cursor.as_ref().and_then(|c| c.last_scheduled_at),
        "cursor_next_fire_at": cursor.as_ref().and_then(|c| c.next_fire_at),
        "cursor_updated_at": cursor.as_ref().map(|c| c.updated_at),
        "cursor_fresh": cursor.is_some(),
        "decision": plan.decision,
        "decision_reason": plan.decision_reason,
        "total_fires": total_fires,
    }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:scheduler/explain",
    endpoint: "scheduler.explain",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, ctx, state).await.map_err(Into::into)
        })
    },
};
