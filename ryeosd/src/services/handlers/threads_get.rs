//! `threads.get` — fetch one thread row plus result, artifacts, and facets.

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
    match state.threads.get_thread(&req.thread_id)? {
        Some(_) => {
            let facets = state.state_store.get_facets(&req.thread_id)?;
            let facets_map: std::collections::HashMap<&str, &str> =
                facets.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
            serde_json::to_value(serde_json::json!({
                "thread": state.threads.get_thread(&req.thread_id)?,
                "result": state.threads.get_thread_result(&req.thread_id)?,
                "artifacts": state.threads.list_thread_artifacts(&req.thread_id)?,
                "facets": facets_map,
            }))
            .map_err(Into::into)
        }
        None => Ok(Value::Null),
    }
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:threads/get",
    endpoint: "threads.get",
    availability: ServiceAvailability::Both,
    required_caps: &[],
    handler: |params, state| {
        Box::pin(async move {
            let req: Request = serde_json::from_value(params)?;
            handle(req, state).await
        })
    },
};
