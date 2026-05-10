//! `scheduler.deregister` — remove a schedule spec.

use std::sync::Arc;

use anyhow::{bail, Result};
use serde::Deserialize;
use serde_json::Value;

use crate::service_executor::ServiceAvailability;
use crate::service_registry::ServiceDescriptor;
use crate::state::AppState;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub schedule_id: String,
}

pub async fn handle(req: Request, state: Arc<AppState>) -> Result<Value> {
    // Check spec exists
    if state.scheduler_db.get_spec(&req.schedule_id)?.is_none() {
        bail!("schedule not found: {}", req.schedule_id);
    }

    // Delete YAML file
    let yaml_path = state.config.system_space_dir
        .join(ryeos_engine::AI_DIR).join("node").join("schedules")
        .join(format!("{}.yaml", req.schedule_id));
    if yaml_path.exists() {
        std::fs::remove_file(&yaml_path)?;
    }

    // Delete projection rows
    state.scheduler_db.delete_spec(&req.schedule_id)?;
    state.scheduler_db.delete_fires_for_schedule(&req.schedule_id)?;

    // JSONL file on disk is preserved (audit trail)

    // Ping timer loop
    if let Some(ref tx) = state.scheduler_reload_tx {
        let _ = tx.try_send(crate::scheduler::ReloadSignal { schedule_id: Some(req.schedule_id.clone()) });
    }

    Ok(serde_json::json!({
        "schedule_id": req.schedule_id,
        "deleted": true,
        "history_preserved": true,
    }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:scheduler/deregister",
    endpoint: "scheduler.deregister",
    availability: ServiceAvailability::Both,
    required_caps: &[],
    handler: |params, state| {
        Box::pin(async move {
            let req: Request = serde_json::from_value(params)?;
            handle(req, state).await
        })
    },
};
