//! `ui.ryeos.thread.tree` — bounded execution ancestry for the focused RyeOS
//! UI tree lens.

use std::sync::Arc;

use anyhow::Result;
use serde::Deserialize;
use serde_json::Value;

use ryeos_api::registry::ServiceDescriptor;
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::handler_error::HandlerError;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

const DEFAULT_MAX_DEPTH: usize = 32;
const MAX_DEPTH: usize = 64;
const DEFAULT_MAX_NODES: usize = 500;
const MAX_NODES: usize = 1_000;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    #[serde(default)]
    thread_id: Option<String>,
    #[serde(default = "default_max_depth")]
    max_depth: usize,
    #[serde(default = "default_max_nodes")]
    max_nodes: usize,
}

const fn default_max_depth() -> usize {
    DEFAULT_MAX_DEPTH
}

const fn default_max_nodes() -> usize {
    DEFAULT_MAX_NODES
}

pub async fn handle(params: Value, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    crate::seat_auth::require_seat_caller(&ctx, &state)?;
    let request: Request = serde_json::from_value(params)
        .map_err(|error| HandlerError::BadRequest(format!("invalid request: {error}")))?;
    let Some(thread_id) = request
        .thread_id
        .as_deref()
        .map(str::trim)
        .filter(|thread_id| !thread_id.is_empty())
    else {
        return Ok(serde_json::json!({
            "root_thread_id": null,
            "threads": [],
            "truncated": false,
        }));
    };
    let tree = state.threads.execution_tree(
        thread_id,
        request.max_depth.clamp(1, MAX_DEPTH),
        request.max_nodes.clamp(1, MAX_NODES),
    )?;
    Ok(serde_json::to_value(tree)?)
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:ui/ryeos-ui/thread/tree",
    endpoint: "ui.ryeos.thread.tree",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| Box::pin(async move { handle(params, ctx, state).await }),
};
