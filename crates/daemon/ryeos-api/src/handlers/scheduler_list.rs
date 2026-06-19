//! `scheduler.list` — list registered schedules.
//!
//! Supports owner filtering: verified callers see only their schedules, and
//! anonymous/unverified callers see none.

use std::sync::Arc;

use anyhow::Result;
use serde::Deserialize;
use serde_json::Value;

use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;
use ryeos_scheduler::planning;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    #[serde(default)]
    pub enabled_only: bool,
    #[serde(default)]
    pub schedule_type: Option<String>,
}

pub async fn handle(
    req: Request,
    ctx: crate::handler_context::HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    let Some(filter_requester) = ctx.is_present().then_some(ctx.fingerprint.as_str()) else {
        // Unverified callers own nothing, so the list is always empty. Say so
        // explicitly — an empty `schedules` here is NOT "nothing is scheduled".
        return Ok(serde_json::json!({
            "schedules": [],
            "identity_filtered": false,
            "anonymous": true,
            "note": "request was not identity-verified; scheduler.list only returns \
                     schedules owned by the calling principal, so an unverified call \
                     always returns []. This does NOT mean nothing is scheduled — sign \
                     the request with the owning key.",
        }));
    };

    let specs = state.scheduler_db.list_specs_filtered(
        req.enabled_only,
        req.schedule_type.as_deref(),
        Some(filter_requester),
    )?;

    // Batch-load last fires to avoid N+1 queries
    let schedule_ids: Vec<String> = specs.iter().map(|s| s.schedule_id.clone()).collect();
    let last_fires = state.scheduler_db.get_last_fires_batch(&schedule_ids)?;
    let cursors = state.scheduler_db.get_cursors_batch(&schedule_ids)?;

    // Batch-load fire counts
    let fire_counts: std::collections::HashMap<String, usize> = schedule_ids
        .iter()
        .filter_map(|id| {
            state
                .scheduler_db
                .count_fires(id, None)
                .ok()
                .map(|c| (id.clone(), c))
        })
        .collect();

    let now = lillux::time::timestamp_millis();
    let schedules: Vec<Value> = specs
        .iter()
        .map(|spec| {
            let last_fire = last_fires.get(&spec.schedule_id);
            let cursor = cursors
                .get(&spec.schedule_id)
                .filter(|cursor| cursor.spec_hash == spec.spec_hash);
            let plan = planning::plan_schedule(spec, last_fire, now);
            let total_fires = fire_counts.get(&spec.schedule_id).copied().unwrap_or(0);
            serde_json::json!({
                "schedule_id": spec.schedule_id,
                "item_ref": spec.item_ref,
                "schedule_type": spec.schedule_type,
                "expression": spec.expression,
                "timezone": spec.timezone,
                "enabled": spec.enabled,
                "registered_at": spec.registered_at,
                "misfire_policy": spec.misfire_policy,
                "overlap_policy": spec.overlap_policy,
                "lateness_grace_secs": spec.lateness_grace_secs,
                "signer_fingerprint": spec.signer_fingerprint,
                "last_scheduled_at": plan.last_scheduled_at,
                "last_fire_at": last_fire.and_then(|f| f.fired_at),
                "last_fire_status": last_fire.map(|f| f.status.clone()),
                "last_fire_reason": last_fire.map(|f| f.trigger_reason.clone()),
                "last_fire_outcome": last_fire.and_then(|f| f.outcome.clone()),
                "current_due_at": plan.current_due_at,
                "current_due_within_grace": plan.current_due_within_grace,
                "misfire_horizon_exclusive": plan.misfire_horizon_exclusive,
                "next_fire_at": plan.next_fire_at,
                "cursor_last_scheduled_at": cursor.and_then(|c| c.last_scheduled_at),
                "cursor_next_fire_at": cursor.and_then(|c| c.next_fire_at),
                "cursor_updated_at": cursor.map(|c| c.updated_at),
                "cursor_fresh": cursor.is_some(),
                "decision": plan.decision,
                "decision_reason": plan.decision_reason,
                "total_fires": total_fires,
            })
        })
        .collect();

    // Always report the filtering identity so an empty result is legible:
    // schedules are owned per-principal (a node-owned schedule belongs to
    // whatever key its signed `execution.requester_fingerprint` names), and the
    // list is scoped to the caller. Without this, an empty list reads as
    // "nothing scheduled" when it really means "nothing owned by you".
    let mut response = serde_json::json!({
        "schedules": schedules,
        "identity_filtered": true,
        "your_fingerprint": filter_requester,
    });
    if response["schedules"]
        .as_array()
        .is_some_and(|a| a.is_empty())
    {
        response["note"] = serde_json::Value::String(format!(
            "no schedules owned by your fingerprint ({filter_requester}); results are \
             scoped to the calling principal. An empty list does NOT mean nothing is \
             scheduled — node-owned schedules belong to whatever principal their signed \
             execution block names. Query as that principal to see them."
        ));
    }
    Ok(response)
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:scheduler/list",
    endpoint: "scheduler.list",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, ctx, state).await
        })
    },
};
