//! `events.chain_replay` — replay events scoped strictly to a `chain_root_id`.
//!
//! Distinct from `events.replay` (thread-scoped). Splitting the two
//! prevents the V5.1 chain-events bug from recurring.
//!
//! Ownership check: non-admin callers can only replay chain events
//! rooted at a thread they own.

use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use ryeos_executor::executor::ServiceAvailability;
use crate::registry::ServiceDescriptor;
use crate::handler_error::HandlerError;
use crate::handler_context::HandlerContext;
use ryeos_app::event_store_service::EventReplayParams;
use ryeos_app::state::AppState;

use super::default_replay_limit;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub chain_root_id: String,
    #[serde(default)]
    pub after_chain_seq: Option<i64>,
    #[serde(default = "default_replay_limit")]
    pub limit: usize,
}

pub async fn handle(req: Request, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value, HandlerError> {
    // Ownership check against the chain root thread.
    let root = state
        .state_store
        .get_thread(&req.chain_root_id)
        .map_err(|e| HandlerError::Internal(e.to_string()))?;

    match root {
        Some(detail) => {
            ctx.require_owner(detail.requested_by.as_deref())?;
        }
        None => return Err(HandlerError::NotFound),
    }

    let result = state.events.replay(&EventReplayParams {
        thread_id: None,
        chain_root_id: Some(req.chain_root_id),
        after_chain_seq: req.after_chain_seq,
        limit: req.limit,
    }).map_err(|e| HandlerError::Internal(e.to_string()))?;

    Ok(serde_json::json!({
        "events": result.events,
        "next_cursor": result.next_cursor,
    }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:events/chain_replay",
    endpoint: "events.chain_replay",
    availability: ServiceAvailability::Both,
    required_caps: &[],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, ctx, state).await.map_err(Into::into)
        })
    },
};
