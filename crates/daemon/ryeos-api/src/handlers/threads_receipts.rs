//! `threads.receipts` — per-node graph receipts for a thread.
//!
//! Ownership-checked exactly like `threads.get`: callers can only inspect
//! their own threads, and a missing/!owned thread returns `null` (not 403) to
//! avoid leaking existence.
//!
//! Returns the thread's `graph_node_receipt` artifacts (written by
//! `crates/runtimes/graph/src/persistence.rs`) — `node`, `step`, `error`,
//! `cache_hit`, `elapsed_ms`, hashes — sorted by `step`, so a graph run can be
//! inspected node-by-node without sifting through every artifact.

use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use crate::handler_context::HandlerContext;
use crate::handler_error::HandlerError;
use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

/// Artifact type the graph runtime writes for each node receipt.
const GRAPH_NODE_RECEIPT: &str = "graph_node_receipt";

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
    let thread = state
        .state_store
        .get_thread(&req.thread_id)
        .map_err(|e| HandlerError::Internal(e.to_string()))?;

    // Mirror `threads.get`: null for missing OR non-owned (after the owner
    // check below), so existence is never leaked.
    let Some(detail) = thread else {
        return Ok(Value::Null);
    };
    ctx.require_owner(detail.requested_by.as_deref())?;

    let artifacts = state
        .threads
        .list_thread_artifacts(&req.thread_id)
        .map_err(|e| HandlerError::Internal(e.to_string()))?;

    let mut receipts: Vec<Value> = artifacts
        .into_iter()
        .filter(|a| a.artifact_type == GRAPH_NODE_RECEIPT)
        .filter_map(|a| a.metadata)
        .collect();
    receipts.sort_by_key(|m| m.get("step").and_then(Value::as_u64).unwrap_or(u64::MAX));

    let count = receipts.len();
    Ok(serde_json::json!({
        "thread_id": req.thread_id,
        "count": count,
        "receipts": receipts,
    }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:threads/receipts",
    endpoint: "threads.receipts",
    availability: ServiceAvailability::Both,
    required_caps: &[],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, ctx, state).await.map_err(Into::into)
        })
    },
};
