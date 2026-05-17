//! `remote/list` — list configured remote nodes.

use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use crate::remote::config;
use ryeos_executor::executor::ServiceAvailability;
use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;

#[derive(serde::Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Request {}

pub async fn handle(_req: Request, state: Arc<AppState>) -> Result<Value> {
    let remotes = config::load_remotes(&state.config.system_space_dir)?;

    let mut entries: Vec<Value> = remotes
        .values()
        .map(|r| {
            serde_json::json!({
                "name": r.name,
                "url": r.url,
                "principal_id": r.principal_id,
            })
        })
        .collect();
    entries.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));

    Ok(serde_json::json!({ "remotes": entries }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:remote/list",
    endpoint: "remote.list",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.remote.list"],
    handler: |params, state| {
        Box::pin(async move {
            let req: Request = if params.is_null() {
                Request::default()
            } else {
                serde_json::from_value(params)?
            };
            handle(req, state).await
        })
    },
};
