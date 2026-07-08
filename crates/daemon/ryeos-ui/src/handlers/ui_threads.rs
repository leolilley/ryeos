//! `ui.ryeos.threads.list` and `ui.ryeos.thread.inspect` — thread
//! listing and read-only inspection for the ryeos-ui.
//!
//! Wraps the existing thread listing from the state store, providing
//! a ryeos-ui-friendly view with status and item_ref data. Browser-session
//! auth means the ryeos-ui always sees all threads (admin context).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use ryeos_api::registry::ServiceDescriptor;
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::handler_error::HandlerError;
use ryeos_app::state::AppState;
use ryeos_app::thread_lifecycle::ThreadListView;
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

fn string_list_filter(params: &Value, key: &str) -> Vec<String> {
    params
        .get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

fn active_filter(params: &Value) -> bool {
    match params.get("active") {
        Some(Value::Bool(active)) => *active,
        Some(Value::String(value)) => matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "active" | "live" | "running"
        ),
        _ => false,
    }
}

pub async fn handle(params: Value, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    let caller = crate::seat_auth::require_seat_caller(&ctx, &state)?;

    let limit = params
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or_else(default_limit)
        .clamp(1, MAX_THREAD_LIST_LIMIT);

    // Optional ordering: the watch dashboard requests `sort: watch`
    // (active-first, then newest), `newest` is newest-first; anything else
    // keeps the default order.
    let sort = match params.get("sort").and_then(|v| v.as_str()) {
        Some("watch") => ryeos_app::thread_lifecycle::ThreadSort::Watch,
        Some("newest") => ryeos_app::thread_lifecycle::ThreadSort::Newest,
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
        facet: string_filter(&params, "facet_key").zip(string_filter(&params, "facet_value")),
        // `active` narrows to live (non-terminal) threads. The filter accepts
        // bools for authored views and text for the TUI live-filter input.
        active_only: active_filter(&params),
    };

    let exclude_item_prefixes = string_list_filter(&params, "exclude_item_prefixes");
    let project_filter = project_filter(&params, caller.project_root())?;
    let needs_post_filter = !exclude_item_prefixes.is_empty() || project_filter.is_some();
    let query_limit = if needs_post_filter {
        MAX_THREAD_LIST_LIMIT
    } else {
        limit
    };

    // Route through the lifecycle layer so each row carries daemon-authored
    // execution facts (`execution.supports_continuation`) the ryeos-ui gates on.
    let mut threads = state
        .threads
        .list_thread_views_query(query_limit, &filter, sort)?;
    if needs_post_filter {
        threads.retain(|row| {
            let item_allowed = !exclude_item_prefixes
                .iter()
                .any(|prefix| row.item.item_ref.starts_with(prefix));
            let project_allowed = project_filter
                .as_ref()
                .is_none_or(|project| row_matches_project(row, project));
            item_allowed && project_allowed
        });
        threads.truncate(limit);
    }

    Ok(serde_json::json!({
        "threads": threads,
    }))
}

fn project_filter(params: &Value, caller_project_root: Option<&str>) -> Result<Option<PathBuf>> {
    let project_path = string_filter(params, "project_path");
    match string_filter(params, "project").as_deref() {
        Some("current") => project_path
            .as_deref()
            .or(caller_project_root)
            .map(canonicalize_project_filter)
            .transpose(),
        Some(path) => canonicalize_existing_dir(path).map(Some),
        None => Ok(None),
    }
}

fn canonicalize_project_filter(path: &str) -> Result<PathBuf> {
    Ok(canonicalize_existing_dir(path).unwrap_or_else(|_| PathBuf::from(path)))
}

fn canonicalize_existing_dir(path: &str) -> Result<PathBuf> {
    let canonical = Path::new(path).canonicalize()?;
    Ok(canonical)
}

fn row_matches_project(row: &ThreadListView, project: &Path) -> bool {
    if let Some(row_project) = &row.project {
        return Path::new(&row_project.path)
            .canonicalize()
            .is_ok_and(|path| path == project);
    }
    is_effectively_active(row)
}

fn is_effectively_active(row: &ThreadListView) -> bool {
    row.follow
        .as_ref()
        .is_some_and(|follow| follow.role == "suspended_parent")
        || !matches!(
            row.item.status.as_str(),
            "completed" | "failed" | "cancelled" | "killed" | "timed_out" | "continued"
        )
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
    let chain_has_usage = totals.input_tokens != 0
        || totals.output_tokens != 0
        || totals.spend_usd != 0.0
        || totals.completed_turns != 0;
    // A rollup-basis final cost (a graph aggregating its dispatched children)
    // lives in the `cost.*` facets, never in `thread_usage` — dispatched
    // children are fresh chain roots, so the chain totals read zero for such
    // a thread. Project the rollup as explicitly-derived rows, and drop the
    // bare-zero chain rows when the rollup is the only usage there is.
    let is_rollup = facets_map.get("cost.basis").map(String::as_str) == Some("rollup");
    let mut usage_rows: Vec<Value> = Vec::new();
    if chain_has_usage || !is_rollup {
        usage_rows.extend([
            serde_json::json!({ "label": "input tokens", "value": totals.input_tokens.to_string() }),
            serde_json::json!({ "label": "output tokens", "value": totals.output_tokens.to_string() }),
            serde_json::json!({ "label": "cost", "value": format!("${:.4}", totals.spend_usd) }),
            serde_json::json!({ "label": "turns", "value": totals.completed_turns.to_string() }),
            serde_json::json!({ "label": "threads in chain", "value": totals.thread_count.to_string() }),
        ]);
    }
    if is_rollup {
        let facet = |key: &str| {
            facets_map
                .get(key)
                .cloned()
                .unwrap_or_else(|| "0".to_string())
        };
        let spend = facets_map
            .get("cost.spend")
            .and_then(|value| value.parse::<f64>().ok())
            .unwrap_or(0.0);
        usage_rows.extend([
            serde_json::json!({ "label": "children input tokens", "value": facet("cost.input_tokens") }),
            serde_json::json!({ "label": "children output tokens", "value": facet("cost.output_tokens") }),
            serde_json::json!({ "label": "children cost (rollup)", "value": format!("${spend:.4}") }),
        ]);
    }
    let usage = Value::Array(usage_rows);

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

    // Graph `follow:` lineage as labeled `{label, value}` rows (same projectable
    // shape as `usage` / `execution_meta`), so the detail lens's Follow section
    // renders `follow_node` / `child_terminal_status` as their own rows instead
    // of dumping the `thread.follow` object. Empty array when the thread carries
    // no follow fact — the section then reads empty.
    let follow = follow_rows(thread.follow.as_ref());

    let mut response = serde_json::json!({
        "schema_version": "ryeos-ui.thread.inspect.v1",
        "thread": thread,
        "result": result,
        "artifacts": artifacts,
        "children": children,
        "facets": facets_map,
        "events": events,
        "usage": usage,
        "pending": pending,
        "execution_meta": execution,
        "follow": follow,
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

/// Build the Follow section's labeled-metric rows (`{label, value}`, the shape
/// the Usage / Execution sections project) from a thread's follow-lineage fact.
/// `None` (a non-follow thread) yields an empty array, so the detail lens's
/// Follow section reads empty rather than dumping the `thread.follow` object.
/// Absent optional fields (e.g. `child_terminal_status` while the child still
/// runs, or the child identities on a durably-recognized successor) drop their
/// row rather than showing a blank.
fn follow_rows(follow: Option<&ryeos_app::thread_lifecycle::FollowFact>) -> Value {
    fn row(label: &str, value: Option<String>) -> Option<Value> {
        value
            .filter(|v| !v.is_empty())
            .map(|value| serde_json::json!({ "label": label, "value": value }))
    }

    let mut rows: Vec<Value> = Vec::new();
    if let Some(f) = follow {
        // `state` (display_state) heads the section: the operator-legible
        // "suspended" / "resumed" the row tones off, distinct from the raw role.
        rows.extend(row("state", Some(f.display_state.to_string())));
        rows.extend(row("phase", f.phase.clone()));
        rows.extend(row("follow node", f.follow_node.clone()));
        rows.extend(row("child chain", f.child_chain_root_id.clone()));
        rows.extend(row("child thread", f.child_thread_id.clone()));
        rows.extend(row("child status", f.child_terminal_status.clone()));
        rows.extend(row(
            "resume successor",
            f.parent_successor_thread_id.clone(),
        ));
    }
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

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:ui/ryeos-ui/threads/list",
    endpoint: "ui.ryeos.threads.list",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| Box::pin(async move { handle(params, ctx, state).await }),
};

pub const INSPECT_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:ui/ryeos-ui/thread/inspect",
    endpoint: "ui.ryeos.thread.inspect",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| Box::pin(async move { handle_inspect(params, ctx, state).await }),
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
        assert_eq!(
            compact_limits(&serde_json::json!("unbounded")),
            "\"unbounded\""
        );
        assert_eq!(compact_limits(&serde_json::json!({})), "{}");
    }

    use ryeos_app::thread_lifecycle::{FollowFact, follow_display_state, follow_role};

    #[test]
    fn follow_rows_none_is_empty() {
        // A non-follow thread contributes no rows — the section reads empty.
        assert!(follow_rows(None).as_array().unwrap().is_empty());
    }

    #[test]
    fn follow_rows_suspended_parent_labels_lineage() {
        let f = FollowFact {
            role: follow_role::SUSPENDED_PARENT,
            display_state: follow_display_state::SUSPENDED,
            phase: Some("waiting".to_string()),
            follow_node: Some("n_follow".to_string()),
            child_thread_id: Some("T-child".to_string()),
            child_chain_root_id: Some("T-child".to_string()),
            // Child still running → no terminal status → the row is dropped.
            child_terminal_status: None,
            parent_successor_thread_id: Some("T-succ".to_string()),
        };
        let rows = follow_rows(Some(&f));
        let by_label: std::collections::HashMap<&str, &str> = rows
            .as_array()
            .unwrap()
            .iter()
            .map(|r| (r["label"].as_str().unwrap(), r["value"].as_str().unwrap()))
            .collect();
        assert_eq!(by_label["state"], "suspended");
        assert_eq!(by_label["phase"], "waiting");
        assert_eq!(by_label["follow node"], "n_follow");
        assert_eq!(by_label["child chain"], "T-child");
        assert_eq!(by_label["resume successor"], "T-succ");
        // The still-running child contributes no terminal-status row.
        assert!(!by_label.contains_key("child status"));
    }

    #[test]
    fn follow_rows_durable_resume_successor_is_state_plus_successor() {
        // The waiter-cleared durable form carries only role/state + successor id;
        // every child-identity row drops rather than showing a blank.
        let f = FollowFact {
            role: follow_role::RESUME_SUCCESSOR,
            display_state: follow_display_state::RESUMED,
            phase: None,
            follow_node: None,
            child_thread_id: None,
            child_chain_root_id: None,
            child_terminal_status: None,
            parent_successor_thread_id: Some("T-succ".to_string()),
        };
        let rows = follow_rows(Some(&f));
        let arr = rows.as_array().unwrap();
        let by_label: std::collections::HashMap<&str, &str> = arr
            .iter()
            .map(|r| (r["label"].as_str().unwrap(), r["value"].as_str().unwrap()))
            .collect();
        assert_eq!(by_label["state"], "resumed");
        assert_eq!(by_label["resume successor"], "T-succ");
        assert_eq!(arr.len(), 2, "only state + successor survive: {arr:?}");
    }
}
