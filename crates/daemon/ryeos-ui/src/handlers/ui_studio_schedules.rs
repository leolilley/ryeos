//! `ui.studio.schedules.list` — schedule listing for the studio.
//!
//! Wraps the existing scheduler listing, providing a studio-friendly
//! view with last-fire and fire-count data.

use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use ryeos_api::registry::ServiceDescriptor;
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::handler_error::HandlerError;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

use crate::state::get_ui_state;

fn session_id_from_context(ctx: &HandlerContext) -> Option<String> {
    ctx.fingerprint.strip_prefix("session:").map(String::from)
}

pub async fn handle(_params: Value, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    let session_id = session_id_from_context(&ctx)
        .ok_or_else(|| HandlerError::Forbidden("browser session required".into()))?;

    get_ui_state(&state)
        .expect("UiState not set")
        .browser_sessions
        .get_session(&session_id)
        .ok_or(HandlerError::Forbidden("session expired or invalid".into()))?;

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
    service_ref: "service:ui/studio/schedules/list",
    endpoint: "ui.studio.schedules.list",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| Box::pin(async move { handle(params, ctx, state).await }),
};

pub const STUDIO_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:ui/studio/schedules/list",
    endpoint: "ui.studio.schedules.list",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| Box::pin(async move { handle(params, ctx, state).await }),
};
