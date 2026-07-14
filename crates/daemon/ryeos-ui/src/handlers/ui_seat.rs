//! Session-authenticated RyeOS UI seat services for browser renderers.
//!
//! The terminal renderer talks directly to `service:seat/*` as a verified
//! operator. Browser renderers arrive through a `session:<id>` wrapper, so
//! these services bind seat ownership to the session's durable principal
//! when present, otherwise to the session id. The persisted object is still
//! a normal `seat_session` thread and the event namespace/shape matches the
//! substrate seat services.

use std::sync::Arc;

use anyhow::Result;
use serde::Deserialize;
use serde_json::{json, Value};

use ryeos_api::registry::ServiceDescriptor;
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::handler_error::HandlerError;
use ryeos_app::state::AppState;
use ryeos_app::state_store::NewEventRecord;
use ryeos_app::thread_lifecycle::{ThreadCreateParams, ThreadFinalizeParams};
use ryeos_executor::executor::ServiceAvailability;

use crate::browser_session::BrowserSession;
use crate::state::get_ui_state;

const SEAT_KIND: &str = "seat_session";
const SEAT_EVENT_PREFIX: &str = "seat.";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OpenRequest {
    surface_ref: String,
    #[serde(default)]
    client_ref: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AppendRequest {
    thread_id: String,
    events: Vec<AppendEvent>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AppendEvent {
    event_type: String,
    #[serde(default)]
    payload: Value,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CloseRequest {
    thread_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplayRequest {
    chain_root_id: String,
    #[serde(default)]
    after_chain_seq: Option<i64>,
    #[serde(default = "default_replay_limit")]
    limit: usize,
}

fn default_replay_limit() -> usize {
    500
}

fn session_id_from_context(ctx: &HandlerContext) -> Option<String> {
    ctx.fingerprint.strip_prefix("session:").map(String::from)
}

fn browser_session(ctx: &HandlerContext, state: &AppState) -> Result<BrowserSession, HandlerError> {
    let session_id = session_id_from_context(ctx)
        .ok_or_else(|| HandlerError::Forbidden("session cookie required".into()))?;
    get_ui_state(state)
        .expect("UiState not set")
        .browser_sessions
        .get_session(&session_id)
        .ok_or_else(|| HandlerError::Forbidden("session expired or invalid".into()))
}

fn seat_owner(session: &BrowserSession) -> String {
    session
        .user_principal_id
        .clone()
        .unwrap_or_else(|| format!("session:{}", session.session_id))
}

fn require_owned_seat(
    state: &AppState,
    thread_id: &str,
    owner: &str,
) -> Result<ryeos_app::state_store::ThreadDetail, HandlerError> {
    let detail = state
        .state_store
        .get_thread(thread_id)
        .map_err(|e| HandlerError::Internal(e.to_string()))?
        .ok_or(HandlerError::NotFound)?;
    if detail.kind != SEAT_KIND {
        return Err(HandlerError::BadRequest(format!(
            "thread {thread_id} is not a seat session"
        )));
    }
    if detail.requested_by.as_deref() != Some(owner) {
        return Err(HandlerError::NotFound);
    }
    Ok(detail)
}

pub async fn handle_open(
    params: Value,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    let session = browser_session(&ctx, &state)?;
    let owner = seat_owner(&session);
    let req: OpenRequest = serde_json::from_value(params)
        .map_err(|e| HandlerError::BadRequest(format!("invalid request: {e}")))?;
    let surface_ref = req.surface_ref;
    let client_ref = req
        .client_ref
        .unwrap_or_else(|| "client:ryeos/web".to_string());

    let existing = state
        .state_store
        .list_threads_filtered(100, Some(&owner))
        .map_err(|e| HandlerError::Internal(e.to_string()))?
        .into_iter()
        .filter(|thread| {
            thread.kind == SEAT_KIND && thread.status == "running" && thread.item_ref == surface_ref
        })
        .max_by(|a, b| a.updated_at.cmp(&b.updated_at));
    if let Some(thread) = existing {
        state
            .state_store
            .touch_seat_lease(&thread.thread_id, &owner, &surface_ref, &client_ref)?;
        return Ok(json!({
            "thread_id": thread.thread_id,
            "chain_root_id": thread.chain_root_id,
            "reattached": true,
        }));
    }

    let thread_id = ryeos_app::thread_lifecycle::new_thread_id();
    let site_id = state.threads.site_id().to_string();
    let detail = state.threads.create_thread(&ThreadCreateParams {
        thread_id: thread_id.clone(),
        chain_root_id: thread_id.clone(),
        kind: SEAT_KIND.to_string(),
        item_ref: surface_ref.clone(),
        executor_ref: client_ref.clone(),
        launch_mode: "inline".to_string(),
        current_site_id: site_id.clone(),
        origin_site_id: site_id,
        upstream_thread_id: None,
        requested_by: Some(owner.clone()),
        project_root: None,
        usage_subject: None,
        usage_subject_asserted_by: None,
    })?;
    state.threads.mark_running(&thread_id)?;
    state
        .state_store
        .touch_seat_lease(&thread_id, &owner, &surface_ref, &client_ref)?;

    Ok(json!({
        "thread_id": detail.thread_id,
        "chain_root_id": detail.chain_root_id,
        "reattached": false,
    }))
}

pub async fn handle_append(
    params: Value,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    let session = browser_session(&ctx, &state)?;
    let owner = seat_owner(&session);
    let req: AppendRequest = serde_json::from_value(params)
        .map_err(|e| HandlerError::BadRequest(format!("invalid request: {e}")))?;
    if req.events.is_empty() {
        return Err(HandlerError::BadRequest("no events to append".into()).into());
    }
    for event in &req.events {
        if !event.event_type.starts_with(SEAT_EVENT_PREFIX) {
            return Err(HandlerError::BadRequest(format!(
                "seat braids accept only `{SEAT_EVENT_PREFIX}*` events, got `{}`",
                event.event_type
            ))
            .into());
        }
    }
    let detail = require_owned_seat(&state, &req.thread_id, &owner)?;
    if detail.status != "running" {
        return Err(HandlerError::BadRequest(format!(
            "seat session is {}; only running seats accept events",
            detail.status
        ))
        .into());
    }
    // Renew before appending so presence either defeats the expiry claim or
    // observes that the reaper already owns the transition.
    state.state_store.touch_seat_lease(
        &detail.thread_id,
        &owner,
        &detail.item_ref,
        &detail.executor_ref,
    )?;

    let records: Vec<NewEventRecord> = req
        .events
        .into_iter()
        .map(|event| NewEventRecord {
            event_type: event.event_type,
            storage_class: "indexed".to_string(),
            payload: event.payload,
        })
        .collect();
    let persisted = state
        .threads
        .append_thread_events(&detail.chain_root_id, &detail.thread_id, &records)?
        .ok_or_else(|| {
            HandlerError::BadRequest(
                "seat session is no longer running; only running seats accept events".into(),
            )
        })?;
    let last_seq = persisted.iter().map(|r| r.chain_seq).max().unwrap_or(0);

    Ok(json!({
        "appended": persisted.len(),
        "chain_seq": last_seq,
    }))
}

pub async fn handle_replay(
    params: Value,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    let session = browser_session(&ctx, &state)?;
    let owner = seat_owner(&session);
    let req: ReplayRequest = serde_json::from_value(params)
        .map_err(|e| HandlerError::BadRequest(format!("invalid request: {e}")))?;
    let detail = require_owned_seat(&state, &req.chain_root_id, &owner)?;
    state.state_store.touch_seat_lease(
        &detail.thread_id,
        &owner,
        &detail.item_ref,
        &detail.executor_ref,
    )?;
    let result = state
        .events
        .replay(&ryeos_app::event_store_service::EventReplayParams {
            thread_id: None,
            chain_root_id: Some(detail.chain_root_id),
            after_chain_seq: req.after_chain_seq,
            limit: req.limit,
        })?;
    let events: Vec<_> = result
        .events
        .into_iter()
        .filter(|event| event.event_type.starts_with(SEAT_EVENT_PREFIX))
        .collect();

    Ok(json!({
        "events": events,
        "next_cursor": result.next_cursor,
    }))
}

pub async fn handle_close(
    params: Value,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    let session = browser_session(&ctx, &state)?;
    let owner = seat_owner(&session);
    let req: CloseRequest = serde_json::from_value(params)
        .map_err(|e| HandlerError::BadRequest(format!("invalid request: {e}")))?;
    let detail = require_owned_seat(&state, &req.thread_id, &owner)?;
    if detail.status != "running" {
        state.state_store.remove_seat_lease(&detail.thread_id)?;
        return Ok(json!({ "thread_id": detail.thread_id, "status": detail.status }));
    }
    let finalized = state.threads.finalize_thread(&ThreadFinalizeParams {
        thread_id: req.thread_id,
        status: "completed".to_string(),
        outcome_code: None,
        result: None,
        error: None,
        metadata: None,
        artifacts: Vec::new(),
        final_cost: None,
        summary_json: None,
    })?;
    state.state_store.remove_seat_lease(&finalized.thread_id)?;
    Ok(json!({ "thread_id": finalized.thread_id, "status": finalized.status }))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TouchRequest {
    thread_id: String,
}

pub async fn handle_touch(
    params: Value,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    let session = browser_session(&ctx, &state)?;
    let owner = seat_owner(&session);
    let req: TouchRequest = serde_json::from_value(params)
        .map_err(|e| HandlerError::BadRequest(format!("invalid request: {e}")))?;
    let detail = require_owned_seat(&state, &req.thread_id, &owner)?;
    if detail.status != "running" {
        state.state_store.remove_seat_lease(&detail.thread_id)?;
        return Err(HandlerError::BadRequest("seat session is not running".into()).into());
    }
    state.state_store.touch_seat_lease(
        &detail.thread_id,
        &owner,
        &detail.item_ref,
        &detail.executor_ref,
    )?;
    Ok(json!({ "thread_id": detail.thread_id, "touched": true }))
}

pub static OPEN_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:ui/seat/open",
    endpoint: "ui.seat.open",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| Box::pin(handle_open(params, ctx, state)),
};

pub static APPEND_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:ui/seat/append",
    endpoint: "ui.seat.append",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| Box::pin(handle_append(params, ctx, state)),
};

pub static REPLAY_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:ui/seat/replay",
    endpoint: "ui.seat.replay",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| Box::pin(handle_replay(params, ctx, state)),
};

pub static CLOSE_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:ui/seat/close",
    endpoint: "ui.seat.close",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| Box::pin(handle_close(params, ctx, state)),
};

pub static TOUCH_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:ui/seat/touch",
    endpoint: "ui.seat.touch",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| Box::pin(handle_touch(params, ctx, state)),
};
