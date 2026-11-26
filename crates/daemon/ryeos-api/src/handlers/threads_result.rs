//! `threads.result` — the body a thread finished with, for the key that owns
//! the thread.
//!
//! `threads.get` names the body only by its digest (`result_digest`), so a
//! peer that is not the node's operator can bind to it without the body being
//! free text in a thread read. This service returns the body itself: one
//! declared content leaf, bounded by the item's `max_result_bytes` and refused
//! when larger, never cut. A thread that is not the caller's answers exactly
//! as a thread that does not exist.

use std::sync::Arc;

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
    let view = state
        .threads
        .get_thread_view(&req.thread_id)
        .map_err(|e| HandlerError::Internal(format!("{e:#}")))?;
    // Not found and not yours are one answer.
    let Some(view) = view else {
        return Err(HandlerError::NotFound);
    };
    ctx.require_owner(view.thread.requested_by.as_deref())?;
    let record = state
        .threads
        .get_thread_result(&req.thread_id)
        .map_err(|e| HandlerError::Internal(format!("{e:#}")))?;
    let body = record.and_then(|record| record.result);
    let digest = body
        .as_ref()
        .map(ryeos_state::objects::canonical_value_digest)
        .transpose()
        .map_err(|e| HandlerError::Internal(format!("{e:#}")))?;
    Ok(serde_json::json!({
        "thread_id": req.thread_id,
        "result_digest": digest,
        "result": body,
    }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:threads/result",
    endpoint: "threads.result",
    availability: ServiceAvailability::Both,
    required_caps: &[],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, ctx, state).await.map_err(Into::into)
        })
    },
};
