//! `threads.children` — list immediate child threads of a parent.
//!
//! Ownership check: callers can only access children of
//! threads they own.

use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use crate::handler_context::HandlerContext;
use crate::handler_error::HandlerError;
use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub thread_id: String,
}

pub async fn handle(
    req: Request,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value, HandlerError> {
    // Ownership check against the parent thread.
    let parent = state
        .state_store
        .get_thread(&req.thread_id)
        .map_err(|e| HandlerError::Internal(e.to_string()))?;

    match parent {
        Some(detail) => {
            ctx.require_owner(detail.requested_by.as_deref())?;
        }
        None => return Err(HandlerError::NotFound),
    }

    let children = state
        .threads
        .list_children(&req.thread_id)
        .map_err(|e| HandlerError::Internal(e.to_string()))?;
    Ok(serde_json::json!({ "children": children }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:threads/children",
    endpoint: "threads.children",
    availability: ServiceAvailability::Both,
    required_caps: &[],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, ctx, state).await.map_err(Into::into)
        })
    },
};
