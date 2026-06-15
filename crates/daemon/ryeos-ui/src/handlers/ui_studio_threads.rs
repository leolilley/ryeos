//! `ui.studio.threads.list` and `ui.studio.thread.inspect` — thread
//! listing and read-only inspection for the studio.
//!
//! Wraps the existing thread listing from the state store, providing
//! a studio-friendly view with status and item_ref data. Browser-session
//! auth means the studio always sees all threads (admin context).

use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use ryeos_api::registry::ServiceDescriptor;
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::handler_error::HandlerError;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

use crate::state::get_ui_state;

fn default_limit() -> usize {
    100
}

const MAX_THREAD_LIST_LIMIT: usize = 2_000;

fn session_id_from_context(ctx: &HandlerContext) -> Option<String> {
    ctx.fingerprint.strip_prefix("session:").map(String::from)
}

fn require_browser_session(
    ctx: &HandlerContext,
    state: &AppState,
) -> Result<crate::browser_session::BrowserSession, HandlerError> {
    let session_id = session_id_from_context(ctx)
        .ok_or_else(|| HandlerError::Forbidden("browser session required".into()))?;

    get_ui_state(state)
        .expect("UiState not set")
        .browser_sessions
        .get_session(&session_id)
        .ok_or(HandlerError::Forbidden("session expired or invalid".into()))
}

pub async fn handle(params: Value, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    crate::seat_auth::require_seat_caller(&ctx, &state)?;

    let limit = params
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or_else(default_limit)
        .clamp(1, MAX_THREAD_LIST_LIMIT);

    // Browser session = admin context: no owner filtering.
    let threads = state.state_store.list_threads_filtered(limit, None)?;

    Ok(serde_json::json!({
        "threads": threads,
    }))
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InspectRequest {
    pub thread_id: String,
    #[serde(default = "default_event_limit")]
    pub event_limit: usize,
}

fn default_event_limit() -> usize {
    100
}

const MAX_EVENT_LIMIT: usize = 500;

pub async fn handle_inspect(
    params: Value,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    crate::seat_auth::require_seat_caller(&ctx, &state)?;

    let req: InspectRequest = serde_json::from_value(params)
        .map_err(|e| HandlerError::BadRequest(format!("invalid request: {e}")))?;

    let Some(thread) = state.state_store.get_thread(&req.thread_id)? else {
        return Err(HandlerError::NotFound.into());
    };

    let result = state.threads.get_thread_result(&req.thread_id)?;
    let artifacts = state.threads.list_thread_artifacts(&req.thread_id)?;
    let children = state.threads.list_children(&req.thread_id)?;
    let facets = state.state_store.get_facets(&req.thread_id)?;
    let facets_map: std::collections::BTreeMap<String, String> = facets.into_iter().collect();
    let events = state
        .state_store
        .latest_thread_events(&req.thread_id, req.event_limit.clamp(1, MAX_EVENT_LIMIT))?;

    Ok(serde_json::json!({
        "schema_version": "studio.thread.inspect.v1",
        "thread": thread,
        "result": result,
        "artifacts": artifacts,
        "children": children,
        "facets": facets_map,
        "events": events,
    }))
}

pub async fn handle_cancel(
    params: Value,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    let session = require_browser_session(&ctx, &state)?;
    if session.read_only {
        return Err(
            HandlerError::Forbidden("read-only session cannot cancel threads".into()).into(),
        );
    }
    let Some(user_principal_id) = session.user_principal_id else {
        return Err(HandlerError::Forbidden(
            "verified user principal required to cancel threads".into(),
        )
        .into());
    };

    let req: ryeos_api::handlers::threads_cancel::Request = serde_json::from_value(params)
        .map_err(|e| HandlerError::BadRequest(format!("invalid request: {e}")))?;
    // Studio cancellation is authorized by the browser session boundary:
    // writable session + server-stored launch principal.  The underlying
    // `threads.cancel` handler still performs the critical owner check
    // against `thread.requested_by` and executes the real cancellation path.
    let owner_ctx = HandlerContext::new(user_principal_id, session.granted_caps, true);
    ryeos_api::handlers::threads_cancel::handle(req, owner_ctx, state)
        .await
        .map_err(Into::into)
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:ui/studio/threads/list",
    endpoint: "ui.studio.threads.list",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| Box::pin(async move { handle(params, ctx, state).await }),
};

pub const INSPECT_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:ui/studio/thread/inspect",
    endpoint: "ui.studio.thread.inspect",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| Box::pin(async move { handle_inspect(params, ctx, state).await }),
};

pub const CANCEL_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:ui/studio/thread/cancel",
    endpoint: "ui.studio.thread.cancel",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| Box::pin(async move { handle_cancel(params, ctx, state).await }),
};
