//! `threads.children` — list immediate child threads of a parent.

use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use crate::service_executor::ServiceAvailability;
use crate::service_registry::ServiceDescriptor;
use crate::state::AppState;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub thread_id: String,
}

pub async fn handle(req: Request, state: Arc<AppState>) -> Result<Value> {
    let children = state.threads.list_children(&req.thread_id)?;
    Ok(serde_json::json!({ "children": children }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:threads/children",
    endpoint: "threads.children",
    availability: ServiceAvailability::Both,
    handler: |params, state| {
        Box::pin(async move {
            let req: Request = serde_json::from_value(params)?;
            handle(req, state).await
        })
    },
};
