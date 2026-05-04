//! `events.replay` — replay events scoped strictly to a single `thread_id`.
//!
//! This is intentionally distinct from `events.chain_replay`. Splitting
//! the two avoids the V5.1 ambiguity bug where a single endpoint could
//! silently switch between thread-scoped and chain-scoped replay based
//! on which optional field was set.

use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use crate::service_executor::ServiceAvailability;
use crate::service_registry::ServiceDescriptor;
use crate::services::event_store::EventReplayParams;
use crate::state::AppState;

use super::default_replay_limit;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub thread_id: String,
    #[serde(default)]
    pub after_chain_seq: Option<i64>,
    #[serde(default = "default_replay_limit")]
    pub limit: usize,
}

pub async fn handle(req: Request, state: Arc<AppState>) -> Result<Value> {
    let result = state.events.replay(&EventReplayParams {
        thread_id: Some(req.thread_id),
        chain_root_id: None,
        after_chain_seq: req.after_chain_seq,
        limit: req.limit,
    })?;
    Ok(serde_json::json!({
        "events": result.events,
        "next_cursor": result.next_cursor,
    }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:events/replay",
    endpoint: "events.replay",
    availability: ServiceAvailability::Both,
    required_caps: &[],
    handler: |params, state| {
        Box::pin(async move {
            let req: Request = serde_json::from_value(params)?;
            handle(req, state).await
        })
    },
};
