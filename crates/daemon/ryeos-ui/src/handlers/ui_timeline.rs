//! `ui.ryeos.timeline.thread` — a chain's events folded into timeline items by
//! a signed definition, returned as the last `limit` top-level items.
//!
//! The fold, its cache and the response shape are the shared host in
//! `ryeos_api::timeline_host`; this endpoint only supplies the browser-seat
//! authorisation. `service:threads/timeline` serves the same response to
//! callers that own the thread.

use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use ryeos_api::registry::ServiceDescriptor;
use ryeos_api::timeline_host::{TimelineParams, timeline_response};
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::handler_error::HandlerError;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

const SERVICE_REF: &str = "service:ui/ryeos-ui/timeline/thread";
const ENDPOINT: &str = "ui.ryeos.timeline.thread";

pub async fn handle(params: Value, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    let caller = crate::seat_auth::require_seat_caller(&ctx, &state)?;
    let params = TimelineParams::parse(params)?;

    // The caller must own the chain's root thread, as `events.chain_replay`
    // requires, judged by the exact-thread subject authority UI reads share.
    let subject = crate::thread_authorization::authorize_exact_thread_subjects(
        &ctx,
        &state,
        &caller,
        &[&params.chain_root_id],
    )?
    .pop()
    .expect("one exact thread authorization returns one signed subject");
    if subject.chain_root_id != params.chain_root_id {
        return Err(HandlerError::NotFound.into());
    }

    timeline_response(&state, &params, subject.status.as_str())
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: SERVICE_REF,
    endpoint: ENDPOINT,
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| Box::pin(async move { handle(params, ctx, state).await }),
};
