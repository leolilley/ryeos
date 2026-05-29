//! `health/status` — daemon liveness + catalog health.
//!
//! Returns the same shape as the old `/health` explicit route,
//! but as a data-driven route served through the dispatcher.

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
    let status = if state.catalog_health.missing_services.is_empty() {
        "healthy"
    } else {
        "degraded"
    };
    Ok(serde_json::json!({
        "status": status,
        "operational_services": state.catalog_health.status,
        "missing_services": state.catalog_health.missing_services,
    }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:health/status",
    endpoint: "health.status",
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
