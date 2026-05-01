//! `events.chain_replay` — replay events scoped strictly to a `chain_root_id`.
//!
//! Distinct from `events.replay` (thread-scoped). Splitting the two
//! prevents the V5.1 chain-events bug from recurring.

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
    pub chain_root_id: String,
    #[serde(default)]
    pub after_chain_seq: Option<i64>,
    #[serde(default = "default_replay_limit")]
    pub limit: usize,
}

pub async fn handle(req: Request, state: Arc<AppState>) -> Result<Value> {
    let result = state.events.replay(&EventReplayParams {
        thread_id: None,
        chain_root_id: Some(req.chain_root_id),
        after_chain_seq: req.after_chain_seq,
        limit: req.limit,
    })?;
    Ok(serde_json::json!({
        "events": result.events,
        "next_cursor": result.next_cursor,
    }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:events/chain_replay",
    endpoint: "events.chain_replay",
    availability: ServiceAvailability::Both,
    handler: |params, state| {
        Box::pin(async move {
            let req: Request = serde_json::from_value(params)?;
            handle(req, state).await
        })
    },
};
