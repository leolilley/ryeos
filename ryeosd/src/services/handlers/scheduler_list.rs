//! `scheduler.list` — list all registered schedules.

use std::sync::Arc;

use anyhow::Result;
use serde::Deserialize;
use serde_json::Value;

use crate::service_executor::ServiceAvailability;
use crate::service_registry::ServiceDescriptor;
use crate::state::AppState;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    #[serde(default)]
    pub enabled_only: bool,
    #[serde(default)]
    pub schedule_type: Option<String>,
}

pub async fn handle(req: Request, state: Arc<AppState>) -> Result<Value> {
    let specs = state.scheduler_db.list_specs(
        req.enabled_only,
        req.schedule_type.as_deref(),
    )?;

    // Batch-load last fires to avoid N+1 queries
    let schedule_ids: Vec<String> = specs.iter().map(|s| s.schedule_id.clone()).collect();
    let last_fires = state.scheduler_db.get_last_fires_batch(&schedule_ids)?;

    let schedules: Vec<Value> = specs.iter().map(|spec| {
        let last_fire = last_fires.get(&spec.schedule_id);
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
            "total_fires": 0, // TODO: count query
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
            let req: Request = serde_json::from_value(params)?;
            handle(req, state).await
        })
    },
};
