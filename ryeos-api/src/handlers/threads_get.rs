//! `threads.get` — fetch one thread row plus result, artifacts, and facets.
//!
//! Ownership check: non-admin callers can only access their own threads.
//! Returns 404 (not 403) to avoid leaking existence.

use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use ryeos_executor::executor::ServiceAvailability;
use crate::registry::ServiceDescriptor;
use crate::handler_error::HandlerError;
use crate::handler_context::HandlerContext;
use ryeos_app::state::AppState;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub thread_id: String,
}

pub async fn handle(req: Request, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value, HandlerError> {
    let thread = state
        .state_store
        .get_thread(&req.thread_id)
        .map_err(|e| HandlerError::Internal(e.to_string()))?;

    match thread {
        Some(detail) => {
            ctx.require_owner(detail.requested_by.as_deref())?;

            let facets = state.state_store.get_facets(&req.thread_id)
                .map_err(|e| HandlerError::Internal(e.to_string()))?;
            let facets_map: std::collections::HashMap<&str, &str> =
                facets.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
            let result = state.threads.get_thread_result(&req.thread_id)
                .map_err(|e| HandlerError::Internal(e.to_string()))?;
            let artifacts = state.threads.list_thread_artifacts(&req.thread_id)
                .map_err(|e| HandlerError::Internal(e.to_string()))?;

            serde_json::to_value(serde_json::json!({
                "thread": detail,
                "result": result,
                "artifacts": artifacts,
                "facets": facets_map,
            }))
            .map_err(|e| HandlerError::Internal(e.to_string()))
        }
        None => Err(HandlerError::NotFound),
    }
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:threads/get",
    endpoint: "threads.get",
    availability: ServiceAvailability::Both,
    required_caps: &[],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, ctx, state).await.map_err(Into::into)
        })
    },
};
