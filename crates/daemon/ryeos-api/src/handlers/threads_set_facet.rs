//! `threads.set_facet` — stamp a `(key, value)` metadata tag on a thread.
//!
//! Event-backed: the tag is appended as a `thread_facet_set` event, so it
//! survives a projection rebuild (a bare facet-table write would vanish — the
//! rebuild clears `thread_facets` and re-derives it from the event log). Cohort
//! / fleet identity is built on this: a launcher stamps `fleet=<run id>` +
//! `game=<id>` post-launch, and `threads.list` filters on it.
//!
//! Ownership: a caller may only tag a thread they own (absent/non-owned both
//! surface as `NotFound` — no existence leak).

use std::sync::Arc;

use anyhow::Result;
use serde_json::{json, Value};

use crate::handler_context::HandlerContext;
use crate::handler_error::HandlerError;
use crate::registry::ServiceDescriptor;
use ryeos_app::event_store_service::{EventAppendItem, EventAppendParams};
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;
use ryeos_runtime::events::RuntimeEventType;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub thread_id: String,
    pub key: String,
    pub value: String,
}

pub async fn handle(
    req: Request,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value, HandlerError> {
    let thread = state
        .state_store
        .get_thread(&req.thread_id)
        .map_err(|e| HandlerError::Internal(e.to_string()))?
        .ok_or(HandlerError::NotFound)?;
    ctx.require_owner(thread.requested_by.as_deref())?;

    if req.key.trim().is_empty() {
        return Err(HandlerError::BadRequest(
            "facet key must not be empty".to_string(),
        ));
    }

    // Enum is the producer-side source of truth for the wire type + storage
    // class; the append path validates it against the closed vocabulary and the
    // projection derives the facet row (survives rebuild).
    let record = state
        .events
        .append(&EventAppendParams {
            thread_id: req.thread_id.clone(),
            event: EventAppendItem {
                event_type: RuntimeEventType::ThreadFacetSet.as_str().to_string(),
                storage_class: RuntimeEventType::ThreadFacetSet
                    .storage_class()
                    .as_str()
                    .to_string(),
                payload: json!({ "key": req.key, "value": req.value }),
            },
        })
        .map_err(|e| HandlerError::Internal(e.to_string()))?;

    serde_json::to_value(record).map_err(|e| HandlerError::Internal(e.to_string()))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:threads/set_facet",
    endpoint: "threads.set_facet",
    availability: ServiceAvailability::Both,
    required_caps: &["ryeos.execute.service.threads/set_facet"],
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

    #[test]
    fn request_requires_all_fields() {
        let ok: Request = serde_json::from_value(json!({
            "thread_id": "T-1", "key": "fleet", "value": "run-9"
        }))
        .unwrap();
        assert_eq!(ok.key, "fleet");
        assert!(
            serde_json::from_value::<Request>(json!({"thread_id": "T-1", "key": "fleet"})).is_err()
        );
        // deny_unknown_fields
        assert!(serde_json::from_value::<Request>(
            json!({"thread_id": "T-1", "key": "f", "value": "v", "x": 1})
        )
        .is_err());
    }

    #[test]
    fn descriptor_route_and_cap() {
        assert_eq!(DESCRIPTOR.endpoint, "threads.set_facet");
        assert_eq!(
            DESCRIPTOR.required_caps,
            &["ryeos.execute.service.threads/set_facet"]
        );
    }
}
