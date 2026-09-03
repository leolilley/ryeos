//! `ingest-ignore` — expose the remote node's ingest ignore rules.
//!
//! Public endpoint (no auth required), like `/public-key`. Used by
//! `remote configure` to cache the remote's ignore rules locally.

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
    let policy = state
        .node_policy
        .require::<ryeos_app::node_policy::sections::ingest_ignore::CompiledIngestIgnorePolicy>(
    )?;
    Ok(serde_json::to_value(&policy.effective_config)?)
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:system/ingest-ignore",
    endpoint: "system.ingest-ignore",
    availability: ServiceAvailability::Both,
    required_caps: &[],
    handler: |params, _ctx, state| {
        Box::pin(async move {
            let req: Request = if params.is_null() {
                Request::default()
            } else {
                crate::handler_error::parse_request(params)?
            };
            handle(req, state).await
        })
    },
};
