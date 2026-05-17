//! `threads.children` — list immediate child threads of a parent.
//!
//! Ownership check: non-admin callers can only access children of
//! threads they own.

use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use ryeos_executor::executor::ServiceAvailability;
use crate::registry::ServiceDescriptor;
use crate::handler_error::HandlerError;
use crate::handlers::ownership::require_owner_or_admin;
use ryeos_app::state::AppState;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub thread_id: String,
    /// Injected by service_invocation for ownership checks.
    #[serde(default)]
    pub _caller_fingerprint: String,
    /// Injected by service_invocation for ownership checks.
    #[serde(default)]
    pub _caller_scopes: Vec<String>,
}

pub async fn handle(req: Request, state: Arc<AppState>) -> Result<Value, HandlerError> {
    // Ownership check against the parent thread.
    let parent = state
        .state_store
        .get_thread(&req.thread_id)
        .map_err(|e| HandlerError::Internal(e.to_string()))?;

    match parent {
        Some(detail) => {
            require_owner_or_admin(
                detail.requested_by.as_deref(),
                &req._caller_fingerprint,
                &req._caller_scopes,
            )?;
        }
        None => return Err(HandlerError::NotFound),
    }

    let children = state.threads.list_children(&req.thread_id)
        .map_err(|e| HandlerError::Internal(e.to_string()))?;
    Ok(serde_json::json!({ "children": children }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:threads/children",
    endpoint: "threads.children",
    availability: ServiceAvailability::Both,
    required_caps: &[],
    handler: |params, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, state).await.map_err(Into::into)
        })
    },
};
