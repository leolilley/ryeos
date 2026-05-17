//! `scheduler.list` — list registered schedules.
//!
//! Supports optional owner filtering: when `_ctx` is
//! present and the caller is not an admin, only schedules owned by
//! the caller are returned.

use std::sync::Arc;

use anyhow::Result;
use serde::Deserialize;
use serde_json::Value;

use ryeos_executor::executor::ServiceAvailability;
use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    #[serde(default)]
    pub enabled_only: bool,
    #[serde(default)]
    pub schedule_type: Option<String>,
    /// Injected by service_invocation / executor. Typed caller context.
    #[serde(default)]
    pub _ctx: crate::handler_context::HandlerContext,
}

pub async fn handle(req: Request, state: Arc<AppState>) -> Result<Value> {
    let filter_requester = if req._ctx.is_present() && !req._ctx.is_admin() {
        Some(req._ctx.fingerprint.as_str())
    } else {
        None
    };

    let specs = state.scheduler_db.list_specs_filtered(
        req.enabled_only,
        req.schedule_type.as_deref(),
        filter_requester,
    )?;

    // Batch-load last fires to avoid N+1 queries
    let schedule_ids: Vec<String> = specs.iter().map(|s| s.schedule_id.clone()).collect();
    let last_fires = state.scheduler_db.get_last_fires_batch(&schedule_ids)?;

    // Batch-load fire counts
    let fire_counts: std::collections::HashMap<String, usize> = schedule_ids.iter()
        .filter_map(|id| state.scheduler_db.count_fires(id, None).ok().map(|c| (id.clone(), c)))
        .collect();

    let schedules: Vec<Value> = specs.iter().map(|spec| {
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
    }).collect();

    Ok(serde_json::json!({ "schedules": schedules }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:scheduler/list",
    endpoint: "scheduler.list",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, state).await
        })
    },
};
