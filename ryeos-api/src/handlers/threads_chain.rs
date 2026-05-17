//! `threads.chain` — return the chain rooted at the given thread.
//!
//! Ownership check: non-admin callers can only access chains rooted
//! at a thread they own.

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
    /// Injected by service_invocation for ownership checks.
    #[serde(default)]
    pub _ctx: HandlerContext,
}

pub async fn handle(req: Request, state: Arc<AppState>) -> Result<Value, HandlerError> {
    // Ownership check against the chain root thread.
    let root = state
        .state_store
        .get_thread(&req.thread_id)
        .map_err(|e| HandlerError::Internal(e.to_string()))?;

    match root {
        Some(detail) => {
            req._ctx.require_owner(detail.requested_by.as_deref())?;
        }
        None => return Err(HandlerError::NotFound),
    }

    match state.threads.get_chain(&req.thread_id)
        .map_err(|e| HandlerError::Internal(e.to_string()))?
    {
        Some(chain) => serde_json::to_value(chain).map_err(|e| HandlerError::Internal(e.to_string())),
        None => Ok(Value::Null),
    }
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:threads/chain",
    endpoint: "threads.chain",
    availability: ServiceAvailability::Both,
    required_caps: &[],
    handler: |params, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, state).await.map_err(Into::into)
        })
    },
};
