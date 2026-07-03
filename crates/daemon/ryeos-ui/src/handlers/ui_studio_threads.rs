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
use ryeos_engine::contracts::ProjectContext;
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

    // Staged operator-input depth (05c.1): lets the input area show an "N staged"
    // chip without a separate service call.
    let pending = state.live_input.pending_len(&req.thread_id);

    // Durable execution posture (05b.v2): the signed `thread.json` audit record
    // (capabilities minted, hard limits, effective trust class, model) written at
    // launch. Absent when the thread carries no live-path launch metadata or the
    // file is gone — then `thread_meta` is omitted and the Execution section
    // reads empty.
    let thread_meta = read_thread_meta(&state, &req.thread_id);
    let execution = execution_meta_rows(thread_meta.as_ref(), &thread.thread.executor_ref);

    let mut response = serde_json::json!({
        "schema_version": "studio.thread.inspect.v1",
        "thread": thread,
        "result": result,
        "artifacts": artifacts,
        "children": children,
        "facets": facets_map,
        "events": events,
        "usage": usage,
        "pending": pending,
        "execution_meta": execution,
    });
    if let Some(meta) = thread_meta {
        response
            .as_object_mut()
            .expect("inspect response is an object")
            .insert("thread_meta".to_string(), meta);
    }
    Ok(response)
}

/// Read the thread's signed `thread.json` audit record from its project and
/// return the parsed (signature-stripped) JSON. `None` when the thread has no
/// live-path launch metadata, its project context is not a local path, or the
/// file is absent/unreadable — all of which mean "no durable posture to show",
/// so the caller omits the field rather than surfacing an error.
fn read_thread_meta(state: &AppState, thread_id: &str) -> Option<Value> {
    let meta = state.state_store.get_launch_metadata(thread_id).ok()??;
    let ctx = meta.resume_context.as_ref()?;
    let project_root = match &ctx.project_context {
        ProjectContext::LocalPath { path } => path,
        _ => return None,
    };
    let path = project_root
        .join(ryeos_engine::AI_DIR)
        .join("state")
        .join("threads")
        .join(thread_id)
        .join("thread.json");
    let signed = std::fs::read_to_string(&path).ok()?;
    let stripped = lillux::signature::strip_signature_lines(&signed);
    serde_json::from_str::<Value>(&stripped).ok()
}

/// Build the Execution section's labeled-metric rows (the same `{label, value}`
/// shape the Usage section projects) from durable data: the `thread.json` audit
/// record for caps/limits/trust/model, and the thread projection's `executor_ref`
/// for the runtime identity. Rows with no value are dropped, so a thread with no
/// audit record yields just the runtime row (or nothing).
fn execution_meta_rows(thread_meta: Option<&Value>, executor_ref: &str) -> Value {
    fn row(label: &str, value: String) -> Option<Value> {
        (!value.is_empty()).then(|| serde_json::json!({ "label": label, "value": value }))
    }

    let mut rows: Vec<Value> = Vec::new();
    if let Some(meta) = thread_meta {
        if let Some(caps) = meta.get("capabilities").and_then(|v| v.as_array()) {
            rows.extend(row("capabilities minted", caps.len().to_string()));
        }
        if let Some(limits) = meta.get("limits").filter(|v| !v.is_null()) {
            rows.extend(row("limits", compact_limits(limits)));
        }
        if let Some(trust) = meta.get("effective_trust_class").and_then(|v| v.as_str()) {
            rows.extend(row("trust class", trust.to_string()));
        }
        if let Some(model) = meta.get("model").and_then(|v| v.as_str()) {
            rows.extend(row("model", model.to_string()));
        }
    }
    // Runtime identity is durable on the thread projection regardless of the
    // audit file, so it shows even when thread.json is absent.
    rows.extend(row("runtime", executor_ref.to_string()));

    Value::Array(rows)
}

/// Render a HardLimits-shaped limits object as a compact `key=value` line for a
/// single projected row. A non-object (or empty) limits value renders as its
/// compact JSON.
fn compact_limits(limits: &Value) -> String {
    match limits.as_object() {
        Some(map) if !map.is_empty() => map
            .iter()
            .map(|(k, v)| format!("{k}={}", compact_scalar(v)))
            .collect::<Vec<_>>()
            .join(", "),
        _ => limits.to_string(),
    }
}

fn compact_scalar(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execution_meta_from_full_audit_record() {
        let meta = serde_json::json!({
            "capabilities": ["ryeos.execute.tool.a", "ryeos.execute.tool.b"],
            "limits": { "turns": 25, "wall_secs": 600 },
            "effective_trust_class": "trusted_bundle",
            "model": "anthropic/claude",
        });
        let rows = execution_meta_rows(Some(&meta), "native:directive");
        let arr = rows.as_array().unwrap();
        let by_label: std::collections::HashMap<&str, &str> = arr
            .iter()
            .map(|r| (r["label"].as_str().unwrap(), r["value"].as_str().unwrap()))
            .collect();
        assert_eq!(by_label["capabilities minted"], "2");
        assert_eq!(by_label["trust class"], "trusted_bundle");
        assert_eq!(by_label["model"], "anthropic/claude");
        assert_eq!(by_label["runtime"], "native:directive");
        // Limits render as a compact key=value line.
        assert!(by_label["limits"].contains("turns=25"));
        assert!(by_label["limits"].contains("wall_secs=600"));
    }

    #[test]
    fn execution_meta_without_audit_record_is_runtime_only() {
        let rows = execution_meta_rows(None, "native:graph");
        let arr = rows.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["label"], serde_json::json!("runtime"));
        assert_eq!(arr[0]["value"], serde_json::json!("native:graph"));
    }

    #[test]
    fn execution_meta_drops_empty_runtime() {
        // A thread with neither an audit record nor an executor ref yields no rows
        // (the section reads empty rather than showing a blank runtime line).
        let rows = execution_meta_rows(None, "");
        assert!(rows.as_array().unwrap().is_empty());
    }

    #[test]
    fn compact_limits_handles_non_object() {
        assert_eq!(compact_limits(&serde_json::json!("unbounded")), "\"unbounded\"");
        assert_eq!(compact_limits(&serde_json::json!({})), "{}");
    }
}
