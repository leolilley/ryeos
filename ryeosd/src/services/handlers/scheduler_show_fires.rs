//! `scheduler.show_fires` — fire history for a schedule.

use std::sync::Arc;

use anyhow::Result;
use serde::Deserialize;
use serde_json::Value;

use crate::service_executor::ServiceAvailability;
use crate::service_registry::ServiceDescriptor;
use crate::state::AppState;

fn default_limit() -> usize { 50 }

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub schedule_id: String,
    #[serde(default = "default_limit")]
    pub limit: usize,
    #[serde(default)]
    pub status: Option<String>,
}

pub async fn handle(req: Request, state: Arc<AppState>) -> Result<Value> {
    let limit = req.limit.clamp(1, 500);
    let (fires, total) = state.scheduler_db.list_fires(
        &req.schedule_id,
        req.status.as_deref(),
        limit,
    )?;

    let fire_entries: Vec<Value> = fires.iter().map(|f| {
        serde_json::json!({
            "fire_id": f.fire_id,
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
            let req: Request = serde_json::from_value(params)?;
            handle(req, state).await
        })
    },
};
