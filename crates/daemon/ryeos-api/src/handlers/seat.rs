//! Seat thread services — the seat is itself a thread.
//!
//! A seat session is launched from a client item, braided, chained, and
//! replayable like every other execution: `seat/open` mints the thread,
//! `seat/append` writes seat facets into its braid (operator-signed,
//! owner-only), `seat/close` settles it. The renderer folds the seat
//! braid for its state; live and replay are the same read.

use std::sync::Arc;

use anyhow::Result;
use serde::Deserialize;
use serde_json::{json, Value};

use ryeos_app::handler_context::HandlerContext;
use ryeos_app::handler_error::HandlerError;
use ryeos_app::state::AppState;
use ryeos_app::state_store::NewEventRecord;
use ryeos_app::thread_lifecycle::{ThreadCreateParams, ThreadFinalizeParams};

use crate::registry::ServiceDescriptor;
use ryeos_executor::executor::ServiceAvailability;

const SEAT_KIND: &str = "seat_session";
const SEAT_EVENT_PREFIX: &str = "seat.";

fn require_operator(ctx: &HandlerContext) -> Result<String, HandlerError> {
    if !ctx.verified || ctx.fingerprint.is_empty() || ctx.fingerprint.starts_with("session:") {
        return Err(HandlerError::Forbidden(
            "verified operator principal required".into(),
        ));
    }
    Ok(ctx.fingerprint.clone())
}

fn require_owned_seat(
    state: &AppState,
    thread_id: &str,
    caller: &str,
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
    if detail.requested_by.as_deref() != Some(caller) {
        return Err(HandlerError::NotFound);
    }
    Ok(detail)
}

// ── seat/open ───────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OpenRequest {
    /// The surface the seat presents (audit subject).
    surface_ref: String,
    /// The client item operating the seat (audit executor).
    #[serde(default)]
    client_ref: Option<String>,
}

pub async fn handle_open(
    params: Value,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    let caller = require_operator(&ctx)?;
    let req: OpenRequest = serde_json::from_value(params)
        .map_err(|e| HandlerError::BadRequest(format!("invalid request: {e}")))?;

    let thread_id = ryeos_app::thread_lifecycle::new_thread_id();
    let site_id = state.threads.site_id().to_string();
    let detail = state.threads.create_thread(&ThreadCreateParams {
        thread_id: thread_id.clone(),
        chain_root_id: thread_id.clone(),
        kind: SEAT_KIND.to_string(),
        item_ref: req.surface_ref,
        executor_ref: req
            .client_ref
            .unwrap_or_else(|| "client:ryeos/tui".to_string()),
        launch_mode: "inline".to_string(),
        current_site_id: site_id.clone(),
        origin_site_id: site_id,
        upstream_thread_id: None,
        requested_by: Some(caller),
        usage_subject: None,
        usage_subject_asserted_by: None,
    })?;
    state.threads.mark_running(&thread_id)?;

    Ok(json!({
        "thread_id": detail.thread_id,
        "chain_root_id": detail.chain_root_id,
    }))
}

// ── seat/append ─────────────────────────────────────────────────────────

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

pub async fn handle_append(
    params: Value,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    let caller = require_operator(&ctx)?;
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
    let detail = require_owned_seat(&state, &req.thread_id, &caller)?;
    if detail.status != "running" {
        return Err(HandlerError::BadRequest(format!(
            "seat session is {}; only running seats accept events",
            detail.status
        ))
        .into());
    }

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

// ── seat/close ──────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CloseRequest {
    thread_id: String,
}

pub async fn handle_close(
    params: Value,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    let caller = require_operator(&ctx)?;
    let req: CloseRequest = serde_json::from_value(params)
        .map_err(|e| HandlerError::BadRequest(format!("invalid request: {e}")))?;
    let detail = require_owned_seat(&state, &req.thread_id, &caller)?;
    if detail.status != "running" {
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
    Ok(json!({ "thread_id": finalized.thread_id, "status": finalized.status }))
}

// ── seat/list ─────────────────────────────────────────────────────────────

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListRequest {
    /// Optional surface filter — only seats presenting this surface.
    #[serde(default)]
    surface_ref: Option<String>,
}

/// List the caller's RUNNING seat sessions, freshest first. Discovery for
/// reattach: the client reattaches the most recent running seat instead of
/// filtering `threads/list` itself — the `seat_session` kind stays daemon-side.
pub async fn handle_list(
    params: Value,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    let caller = require_operator(&ctx)?;
    let req: ListRequest = if params.is_null() {
        ListRequest::default()
    } else {
        serde_json::from_value(params)
            .map_err(|e| HandlerError::BadRequest(format!("invalid request: {e}")))?
    };
    let mut seats: Vec<_> = state
        .state_store
        .list_threads_filtered(200, Some(&caller))
        .map_err(|e| HandlerError::Internal(e.to_string()))?
        .into_iter()
        .filter(|thread| thread.kind == SEAT_KIND && thread.status == "running")
        .filter(|thread| {
            req.surface_ref
                .as_deref()
                .map_or(true, |surface| thread.item_ref == surface)
        })
        .collect();
    // Freshest first, so the client reattaches the most recent running seat.
    seats.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    let seats: Vec<Value> = seats
        .into_iter()
        .map(|thread| {
            json!({
                "thread_id": thread.thread_id,
                "chain_root_id": thread.chain_root_id,
                "surface_ref": thread.item_ref,
                "updated_at": thread.updated_at,
            })
        })
        .collect();
    Ok(json!({ "seats": seats }))
}

// ── descriptors ─────────────────────────────────────────────────────────

pub static OPEN_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:seat/open",
    endpoint: "seat.open",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.seat/open"],
    handler: |params, ctx, state| Box::pin(handle_open(params, ctx, state)),
};

pub static LIST_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:seat/list",
    endpoint: "seat.list",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.seat/list"],
    handler: |params, ctx, state| Box::pin(handle_list(params, ctx, state)),
};

pub static APPEND_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:seat/append",
    endpoint: "seat.append",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.seat/append"],
    handler: |params, ctx, state| Box::pin(handle_append(params, ctx, state)),
};

pub static CLOSE_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:seat/close",
    endpoint: "seat.close",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.seat/close"],
    handler: |params, ctx, state| Box::pin(handle_close(params, ctx, state)),
};
