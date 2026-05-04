//! `system.status` — daemon health snapshot.

use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use crate::service_executor::ServiceAvailability;
use crate::service_registry::ServiceDescriptor;
use crate::state::AppState;

#[derive(serde::Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Request {}

pub async fn handle(_req: Request, state: Arc<AppState>) -> Result<Value> {
    serde_json::to_value(state.status()).map_err(Into::into)
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:system/status",
    endpoint: "system.status",
    availability: ServiceAvailability::Both,
    required_caps: &[],
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
