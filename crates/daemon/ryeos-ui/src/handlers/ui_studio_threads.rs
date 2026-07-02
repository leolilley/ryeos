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


fn default_limit() -> usize {
    100
}

const MAX_THREAD_LIST_LIMIT: usize = 2_000;

/// Read an optional string filter param: absent, non-string, or empty/blank all
/// mean "unfiltered" (`None`). The client sends `""` for an unset filter facet,
/// so this is where that collapses to no filter.
fn string_filter(params: &Value, key: &str) -> Option<String> {
    params
        .get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

pub async fn handle(params: Value, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    crate::seat_auth::require_seat_caller(&ctx, &state)?;

    let limit = params
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or_else(default_limit)
        .clamp(1, MAX_THREAD_LIST_LIMIT);

    // Optional ordering: the watch dashboard requests `sort: watch`
    // (active-first, then newest); anything else keeps the default order.
    let sort = match params.get("sort").and_then(|v| v.as_str()) {
        Some("watch") => ryeos_app::thread_lifecycle::ThreadSort::Watch,
        _ => ryeos_app::thread_lifecycle::ThreadSort::Default,
    };

    // Optional dashboard filters. An empty or absent value means "unfiltered"
    // (the client sends "" for an unset filter facet), so an unset filter
    // widens the list rather than emptying it. Seat auth is unchanged: a
    // browser session is admin context, so there is no owner (`principal`)
    // scope — these are operator-chosen facets, not an authorization boundary.
    let filter = ryeos_app::thread_lifecycle::ThreadListFilter {
        principal: None,
        status: string_filter(&params, "status"),
        kind: string_filter(&params, "kind"),
        requested_by: string_filter(&params, "requested_by"),
    };

    // Route through the lifecycle layer so each row carries daemon-authored
    // execution facts (`execution.supports_continuation`) the studio gates on.
    let threads = state
        .threads
        .list_thread_views_query(limit, &filter, sort)?;

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

    let Some(thread) = state.threads.get_thread_view(&req.thread_id)? else {
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

    // Deep-watch execution summary: chain-wide usage totals (this thread plus
    // its continuations) as a list of labeled metrics the detail lens projects
    // one row each. Kept as a projectable array (not the flat `cost.*` facets,
    // whose dotted keys a view projection can't navigate).
    let totals = state
        .state_store
        .chain_usage_totals(&thread.thread.chain_root_id)?;
    let usage = serde_json::json!([
        { "label": "input tokens", "value": totals.input_tokens.to_string() },
        { "label": "output tokens", "value": totals.output_tokens.to_string() },
        { "label": "cost", "value": format!("${:.4}", totals.spend_usd) },
        { "label": "turns", "value": totals.completed_turns.to_string() },
        { "label": "threads in chain", "value": totals.thread_count.to_string() },
    ]);

    Ok(serde_json::json!({
        "schema_version": "studio.thread.inspect.v1",
        "thread": thread,
        "result": result,
        "artifacts": artifacts,
        "children": children,
        "facets": facets_map,
        "events": events,
        "usage": usage,
    }))
}

pub async fn handle_cancel(
    params: Value,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    use crate::seat_auth::SeatCaller;

    let req: ryeos_api::handlers::threads_cancel::Request = serde_json::from_value(params)
        .map_err(|e| HandlerError::BadRequest(format!("invalid request: {e}")))?;

    // Accept both seat lanes, like the list/inspect studio services: a browser
    // session (writable + server-stored launch principal) OR a verified operator
    // (e.g. the terminal watch dashboard). Either way the underlying
    // `threads.cancel` still owner-checks `thread.requested_by` and runs the real
    // cancellation.
    let owner_ctx = match crate::seat_auth::require_seat_caller(&ctx, &state)? {
        SeatCaller::Session(session) => {
            if session.read_only {
                return Err(HandlerError::Forbidden(
                    "read-only session cannot cancel threads".into(),
                )
                .into());
            }
            let user_principal_id = session.user_principal_id.ok_or_else(|| {
                HandlerError::Forbidden(
                    "verified user principal required to cancel threads".into(),
                )
            })?;
            HandlerContext::new(user_principal_id, session.granted_caps, true)
        }
        // Verified operator: pass the caller context straight through — the
        // owner check happens in threads.cancel.
        SeatCaller::Operator { .. } => ctx,
    };
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
