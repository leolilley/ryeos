//! `threads.chain` — return the chain rooted at the given thread.

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
    match state.threads.get_chain(&req.thread_id)? {
        Some(chain) => serde_json::to_value(chain).map_err(Into::into),
        None => Ok(Value::Null),
    }
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:threads/chain",
    endpoint: "threads.chain",
    availability: ServiceAvailability::Both,
    handler: |params, state| {
        Box::pin(async move {
            let req: Request = serde_json::from_value(params)?;
            handle(req, state).await
        })
    },
};
