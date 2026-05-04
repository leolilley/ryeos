//! `threads.list` — paginated thread listing.

use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use crate::service_executor::ServiceAvailability;
use crate::service_registry::ServiceDescriptor;
use crate::state::AppState;

use super::default_list_limit;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    #[serde(default = "default_list_limit")]
    pub limit: usize,
}

pub async fn handle(req: Request, state: Arc<AppState>) -> Result<Value> {
    state.threads.list_threads(req.limit)
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:threads/list",
    endpoint: "threads.list",
    availability: ServiceAvailability::Both,
    required_caps: &[],
    handler: |params, state| {
        Box::pin(async move {
            let req: Request = serde_json::from_value(params)?;
            handle(req, state).await
        })
    },
};
