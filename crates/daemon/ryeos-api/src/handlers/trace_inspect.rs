//! `trace.inspect` — read-only normalized view over durable event replay.
//!
//! Every normalized row carries a verifiable event reference, and the raw
//! persisted event remains available for domain-specific consumers.

use std::sync::Arc;

use anyhow::Result;
use serde::Serialize;
use serde_json::{json, Value};

use crate::handler_context::HandlerContext;
use crate::handler_error::HandlerError;
use crate::registry::ServiceDescriptor;
use ryeos_app::event_store_service::EventReplayParams;
use ryeos_app::state::AppState;
use ryeos_app::state_store::PersistedEventRecord;
use ryeos_executor::executor::ServiceAvailability;

use super::default_replay_limit;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    #[serde(default)]
    pub thread_id: Option<String>,
    #[serde(default)]
    pub chain_root_id: Option<String>,
    #[serde(default)]
    pub after_chain_seq: Option<i64>,
    #[serde(default = "default_replay_limit")]
    pub limit: usize,
}

#[derive(Debug, Clone, Serialize)]
struct EventRef {
    chain_root_id: String,
    thread_id: String,
    chain_seq: i64,
    thread_seq: i64,
    event_hash: String,
    event_type: String,
}

#[derive(Debug, Clone, Serialize)]
struct TraceEvent {
    event_ref: EventRef,
    event_type: String,
    ts: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    state_anchor: Option<Value>,
    raw_event: PersistedEventRecord,
}

#[derive(Debug, Clone, Serialize)]
struct StateAnchor {
    event_ref: EventRef,
    anchor: Value,
}

fn event_ref(event: &PersistedEventRecord) -> Result<EventRef, HandlerError> {
    let event_hash = event.event_hash.clone().ok_or_else(|| {
        HandlerError::Internal(format!(
            "durable event {} in chain {} has no event_hash",
            event.chain_seq, event.chain_root_id
        ))
    })?;
    Ok(EventRef {
        chain_root_id: event.chain_root_id.clone(),
        thread_id: event.thread_id.clone(),
        chain_seq: event.chain_seq,
        thread_seq: event.thread_seq,
        event_hash,
        event_type: event.event_type.clone(),
    })
}

fn state_anchor_payload(event: &PersistedEventRecord) -> Option<Value> {
    if event.event_type != "milestone" {
        return None;
    }
    if event.payload.get("kind").and_then(Value::as_str) != Some("state_anchor") {
        return None;
    }
    event.payload.get("payload").cloned()
}

pub async fn handle(
    req: Request,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value, HandlerError> {
    match (&req.thread_id, &req.chain_root_id) {
        (Some(_), Some(_)) | (None, None) => {
            return Err(HandlerError::BadRequest(
                "trace.inspect requires exactly one of thread_id or chain_root_id".to_string(),
            ));
        }
        _ => {}
    }

    let (chain_root_id, thread_id) = match (&req.chain_root_id, &req.thread_id) {
        (Some(chain_root_id), None) => {
            let root = state
                .state_store
                .get_thread(chain_root_id)
                .map_err(|e| HandlerError::Internal(e.to_string()))?;
            match root {
                Some(detail) => ctx.require_owner(detail.requested_by.as_deref())?,
                None => return Err(HandlerError::NotFound),
            }
            (chain_root_id.clone(), None)
        }
        (None, Some(thread_id)) => {
            let thread = state
                .state_store
                .get_thread(thread_id)
                .map_err(|e| HandlerError::Internal(e.to_string()))?;
            match thread {
                Some(detail) => {
                    ctx.require_owner(detail.requested_by.as_deref())?;
                    (detail.chain_root_id, Some(thread_id.clone()))
                }
                None => return Err(HandlerError::NotFound),
            }
        }
        _ => unreachable!("validated above"),
    };

    let result = state
        .events
        .replay(&EventReplayParams {
            thread_id: thread_id.clone(),
            chain_root_id: Some(chain_root_id.clone()),
            after_chain_seq: req.after_chain_seq,
            limit: req.limit,
        })
        .map_err(|e| HandlerError::Internal(e.to_string()))?;

    let mut events = Vec::with_capacity(result.events.len());
    let mut state_anchors = Vec::new();
    for raw_event in result.events {
        let reference = event_ref(&raw_event)?;
        let anchor = state_anchor_payload(&raw_event);
        if let Some(anchor_value) = anchor.clone() {
            state_anchors.push(StateAnchor {
                event_ref: reference.clone(),
                anchor: anchor_value,
            });
        }
        events.push(TraceEvent {
            event_ref: reference,
            event_type: raw_event.event_type.clone(),
            ts: raw_event.ts.clone(),
            state_anchor: anchor,
            raw_event,
        });
    }

    Ok(json!({
        "scope": {
            "chain_root_id": chain_root_id,
            "thread_id": thread_id,
        },
        "events": events,
        "state_anchors": state_anchors,
        "next_cursor": result.next_cursor,
    }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:trace/inspect",
    endpoint: "trace.inspect",
    availability: ServiceAvailability::Both,
    required_caps: &[],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, ctx, state).await.map_err(Into::into)
        })
    },
};

#[cfg(test)]
mod tests {
    use super::*;

    fn persisted_event(event_type: &str, payload: Value) -> PersistedEventRecord {
        PersistedEventRecord {
            event_id: 1,
            event_hash: Some("a".repeat(64)),
            chain_root_id: "T-root".to_string(),
            chain_seq: 1,
            thread_id: "T-root".to_string(),
            thread_seq: 1,
            event_type: event_type.to_string(),
            storage_class: "indexed".to_string(),
            ts: "2026-07-08T00:00:00Z".to_string(),
            prev_chain_event_hash: None,
            prev_thread_event_hash: None,
            payload,
        }
    }

    #[test]
    fn state_anchor_payload_uses_nested_milestone_shape() {
        let event = persisted_event(
            "milestone",
            json!({
                "kind": "state_anchor",
                "payload": {
                    "schema_version": 1,
                    "label": "arc.sim_state",
                    "state_digest": "sha256:test"
                },
                "node": "n1",
                "step": 3
            }),
        );

        let anchor = state_anchor_payload(&event).expect("state anchor");
        assert_eq!(anchor["label"], "arc.sim_state");
        assert_eq!(anchor.get("node"), None);
    }

    #[test]
    fn event_ref_requires_durable_hash() {
        let mut event = persisted_event("milestone", json!({}));
        assert_eq!(event_ref(&event).unwrap().event_hash, "a".repeat(64));

        event.event_hash = None;
        assert!(matches!(event_ref(&event), Err(HandlerError::Internal(_))));
    }
}
