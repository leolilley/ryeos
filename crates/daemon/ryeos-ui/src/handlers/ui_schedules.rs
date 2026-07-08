//! `ui.ryeos.schedules.list` — schedule listing for the ryeos-ui.
//!
//! Wraps the existing scheduler listing, providing a ryeos-ui-friendly
//! view with last-fire and fire-count data.

use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use ryeos_api::registry::ServiceDescriptor;
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

pub async fn handle(_params: Value, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    crate::seat_auth::require_seat_caller(&ctx, &state)?;

    // Browser session = admin: no owner filtering, include disabled.
    let specs = state.scheduler_db.list_specs_filtered(false, None, None)?;

    // Batch-load last fires to avoid N+1 queries
    let schedule_ids: Vec<String> = specs.iter().map(|s| s.schedule_id.clone()).collect();
    let last_fires = state.scheduler_db.get_last_fires_batch(&schedule_ids)?;

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

    let schedules: Vec<Value> = specs
        .iter()
        .map(|spec| {
            let last_fire = last_fires.get(&spec.schedule_id);
            let total_fires = fire_counts.get(&spec.schedule_id).copied().unwrap_or(0);
            serde_json::json!({
                "schedule_id": spec.schedule_id,
                "item_ref": spec.item_ref,
                "schedule_type": spec.schedule_type,
                "expression": spec.expression,
                "timezone": spec.timezone,
                "enabled": spec.enabled,
                "signer_fingerprint": spec.signer_fingerprint,
                "last_fire_at": last_fire.and_then(|f| f.fired_at),
                "last_fire_status": last_fire.map(|f| f.status.clone()),
                "total_fires": total_fires,
            })
        })
        .collect();

    Ok(serde_json::json!({ "schedules": schedules }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:ui/ryeos-ui/schedules/list",
    endpoint: "ui.ryeos.schedules.list",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| Box::pin(async move { handle(params, ctx, state).await }),
};
