//! `events.replay` — replay events scoped strictly to a single `thread_id`.
//!
//! This is intentionally distinct from `events.chain_replay`. Splitting
//! the two avoids the V5.1 ambiguity bug where a single endpoint could
//! silently switch between thread-scoped and chain-scoped replay based
//! on which optional field was set.
//!
//! Ownership check: non-admin callers can only replay events for
//! threads they own.

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
    pub thread_id: String,
    #[serde(default)]
    pub after_chain_seq: Option<i64>,
    #[serde(default = "default_replay_limit")]
    pub limit: usize,
}

pub async fn handle(req: Request, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value, HandlerError> {
    // Ownership check against the thread.
    let thread = state
        .state_store
        .get_thread(&req.thread_id)
        .map_err(|e| HandlerError::Internal(e.to_string()))?;

    match thread {
        Some(detail) => {
            ctx.require_owner(detail.requested_by.as_deref())?;
        }
        None => return Err(HandlerError::NotFound),
    }

    let result = state.events.replay(&EventReplayParams {
        thread_id: Some(req.thread_id),
        chain_root_id: None,
        after_chain_seq: req.after_chain_seq,
        limit: req.limit,
    }).map_err(|e| HandlerError::Internal(e.to_string()))?;

    Ok(serde_json::json!({
        "events": result.events,
        "next_cursor": result.next_cursor,
    }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:events/replay",
    endpoint: "events.replay",
    availability: ServiceAvailability::Both,
    required_caps: &[],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, ctx, state).await.map_err(Into::into)
        })
    },
};
