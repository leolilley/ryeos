//! `bundle.list` — enumerate registered bundles from the node-config snapshot.

use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

#[derive(serde::Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Request {}

pub async fn handle(_req: Request, state: Arc<AppState>) -> Result<Value> {
    let mut bundles = Vec::new();
    for record in &state.node_config.bundles {
        bundles.push(serde_json::json!({
            "name": record.name,
            "path": record.path.display().to_string(),
        }));
    }
    Ok(serde_json::json!({ "bundles": bundles }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:bundle/list",
    endpoint: "bundle.list",
    availability: ServiceAvailability::Both,
    required_caps: &[],
    handler: |params, _ctx, state| {
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
