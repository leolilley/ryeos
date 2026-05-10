//! `scheduler.resume` — re-enable a paused schedule.

use std::sync::Arc;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;

use crate::node_config::writer;
use crate::service_executor::ServiceAvailability;
use crate::service_registry::ServiceDescriptor;
use crate::state::AppState;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub schedule_id: String,
}

pub async fn handle(req: Request, state: Arc<AppState>) -> Result<Value> {
    let spec = state.scheduler_db.get_spec(&req.schedule_id)?
        .ok_or_else(|| anyhow::anyhow!("schedule not found: {}", req.schedule_id))?;

    if spec.enabled {
        return Ok(serde_json::json!({
            "schedule_id": req.schedule_id,
            "enabled": true,
        }));
    }

    let node_dir = state.config.system_space_dir.join(ryeos_engine::AI_DIR).join("node");
    let yaml_path = node_dir
        .join("schedules")
        .join(format!("{}.yaml", req.schedule_id));

    let content = std::fs::read_to_string(&yaml_path)
        .with_context(|| format!("read schedule YAML {}", yaml_path.display()))?;

    let body_str = strip_signature(&content);
    let mut body: serde_json::Value = serde_yaml::from_str(&body_str)?;
    body["enabled"] = Value::Bool(true);

    writer::write_signed_node_item(
        &node_dir,
        "schedules",
        &req.schedule_id,
        &body,
        &state.identity,
    )?;

    let mut rec = spec;
    rec.enabled = true;
    rec.last_modified = lillux::time::timestamp_millis();
    state.scheduler_db.upsert_spec(&rec)?;

    if let Some(ref tx) = state.scheduler_reload_tx {
        let _ = tx.try_send(crate::scheduler::ReloadSignal { schedule_id: Some(req.schedule_id.clone()) });
    }

    Ok(serde_json::json!({
        "schedule_id": req.schedule_id,
        "enabled": true,
    }))
}

fn strip_signature(content: &str) -> String {
    content
        .lines()
        .skip_while(|l| l.trim().starts_with("# ryeos:signed:"))
        .collect::<Vec<_>>()
        .join("\n")
        .trim_start()
        .to_string()
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:scheduler/resume",
    endpoint: "scheduler.resume",
    availability: ServiceAvailability::Both,
    required_caps: &[],
    handler: |params, state| {
        Box::pin(async move {
            let req: Request = serde_json::from_value(params)?;
            handle(req, state).await
        })
    },
};
