//! `trace.branch` — daemon-authored signed branch provenance.

use std::sync::Arc;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::handler_context::HandlerContext;
use crate::handler_error::HandlerError;
use crate::registry::ServiceDescriptor;
use ryeos_app::event_store_service::EventReplayParams;
use ryeos_app::state::AppState;
use ryeos_app::state_store::NewThreadRecord;
use ryeos_app::thread_lifecycle::new_thread_id;
use ryeos_executor::executor::ServiceAvailability;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EventRef {
    pub chain_root_id: String,
    pub thread_id: String,
    pub chain_seq: i64,
    pub thread_seq: i64,
    pub event_hash: String,
    pub event_type: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub parent_event_ref: EventRef,
    pub state_anchor_ref: EventRef,
    #[serde(default)]
    pub child_thread_id: Option<String>,
    #[serde(default)]
    pub purpose: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub item_ref: Option<String>,
    #[serde(default)]
    pub executor_ref: Option<String>,
    #[serde(default)]
    pub launch_mode: Option<String>,
    #[serde(default)]
    pub restore_contract: Option<Value>,
    #[serde(default)]
    pub metadata: Value,
}

fn verify_ref(actual: &ryeos_app::state_store::PersistedEventRecord, expected: &EventRef) -> bool {
    actual.chain_root_id == expected.chain_root_id
        && actual.thread_id == expected.thread_id
        && actual.chain_seq == expected.chain_seq
        && actual.thread_seq == expected.thread_seq
        && actual.event_type == expected.event_type
        && actual.event_hash.as_deref() == Some(expected.event_hash.as_str())
}

fn is_state_anchor(event: &ryeos_app::state_store::PersistedEventRecord) -> bool {
    event.event_type == "milestone"
        && event.payload.get("kind").and_then(Value::as_str) == Some("state_anchor")
        && event.payload.get("payload").is_some()
}

fn durable_event_by_ref(
    state: &AppState,
    expected: &EventRef,
) -> Result<ryeos_app::state_store::PersistedEventRecord, HandlerError> {
    if expected.chain_seq <= 0 || expected.thread_seq <= 0 {
        return Err(HandlerError::BadRequest(
            "event refs require positive chain_seq and thread_seq".to_string(),
        ));
    }
    let result = state
        .events
        .replay(&EventReplayParams {
            thread_id: Some(expected.thread_id.clone()),
            chain_root_id: Some(expected.chain_root_id.clone()),
            after_chain_seq: Some(expected.chain_seq - 1),
            limit: 1,
        })
        .map_err(|e| HandlerError::Internal(e.to_string()))?;
    let Some(event) = result.events.into_iter().next() else {
        return Err(HandlerError::BadRequest(format!(
            "event ref not found at chain_seq {}",
            expected.chain_seq
        )));
    };
    if verify_ref(&event, expected) {
        Ok(event)
    } else {
        Err(HandlerError::BadRequest(
            "event ref does not match durable replay".to_string(),
        ))
    }
}

pub async fn handle(
    req: Request,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value, HandlerError> {
    ctx.require_verified()?;

    if req.parent_event_ref.chain_root_id != req.state_anchor_ref.chain_root_id {
        return Err(HandlerError::BadRequest(
            "parent_event_ref and state_anchor_ref must be in the same chain".to_string(),
        ));
    }

    let parent_thread = state
        .state_store
        .get_thread(&req.parent_event_ref.thread_id)
        .map_err(|e| HandlerError::Internal(e.to_string()))?
        .ok_or(HandlerError::NotFound)?;
    ctx.require_owner(parent_thread.requested_by.as_deref())?;
    if parent_thread.chain_root_id != req.parent_event_ref.chain_root_id {
        return Err(HandlerError::BadRequest(
            "parent_event_ref chain_root_id does not match parent thread".to_string(),
        ));
    }

    let parent_event = durable_event_by_ref(&state, &req.parent_event_ref)?;
    let state_anchor_event = durable_event_by_ref(&state, &req.state_anchor_ref)?;
    if !is_state_anchor(&state_anchor_event) {
        return Err(HandlerError::BadRequest(
            "state_anchor_ref must point to a state_anchor milestone".to_string(),
        ));
    }

    let child_thread_id = req.child_thread_id.unwrap_or_else(new_thread_id);
    if let Some(existing) = state
        .state_store
        .get_thread(&child_thread_id)
        .map_err(|e| HandlerError::Internal(e.to_string()))?
    {
        if existing.chain_root_id == req.parent_event_ref.chain_root_id {
            return Err(HandlerError::Conflict(format!(
                "branch child thread already exists: {child_thread_id}"
            )));
        }
        return Err(HandlerError::BadRequest(format!(
            "branch child thread id belongs to another chain: {child_thread_id}"
        )));
    }

    let parent_generation = state
        .state_store
        .authoritative_project_generation(&parent_thread.thread_id)
        .map_err(|e| HandlerError::Internal(e.to_string()))?
        .ok_or_else(|| HandlerError::Internal("parent authoritative history is missing".into()))?;
    let branch_thread = NewThreadRecord {
        thread_id: child_thread_id.clone(),
        chain_root_id: req.parent_event_ref.chain_root_id.clone(),
        kind: req.kind.unwrap_or(parent_thread.kind),
        item_ref: req.item_ref.unwrap_or(parent_thread.item_ref),
        executor_ref: req.executor_ref.unwrap_or(parent_thread.executor_ref),
        launch_mode: req.launch_mode.unwrap_or(parent_thread.launch_mode),
        current_site_id: parent_thread.current_site_id,
        origin_site_id: parent_thread.origin_site_id,
        upstream_thread_id: None,
        requested_by: Some(ctx.fingerprint.clone()),
        project_root: parent_thread
            .project_root
            .as_ref()
            .map(std::path::PathBuf::from),
        base_project_snapshot_hash: parent_generation.1.or(parent_generation.0),
        usage_subject: None,
        usage_subject_asserted_by: None,
        captured_history_policy: None,
    };
    let branch_payload = json!({
        "relation": "trace_branch",
        "child_thread_id": child_thread_id,
        "parent_event_ref": req.parent_event_ref,
        "state_anchor_ref": req.state_anchor_ref,
        "parent_event_type": parent_event.event_type,
        "purpose": req.purpose.unwrap_or_else(|| "trace_branch".to_string()),
        "restore_contract": req.restore_contract,
        "metadata": req.metadata,
    });
    let branch_events = state
        .state_store
        .create_trace_branch(&branch_thread, branch_payload)
        .map_err(|e| HandlerError::Internal(e.to_string()))?;
    state.event_streams.publish_ordered(&branch_events);
    let detail = state
        .threads
        .get_thread(&branch_thread.thread_id)
        .map_err(|e| HandlerError::Internal(e.to_string()))?
        .ok_or_else(|| HandlerError::Internal("created branch thread missing".to_string()))?;

    Ok(json!({
        "thread": detail,
        "branch_events": branch_events,
    }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:trace/branch",
    endpoint: "trace.branch",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.trace/branch"],
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
    use ryeos_app::state_store::PersistedEventRecord;

    fn persisted_event(event_type: &str, payload: Value) -> PersistedEventRecord {
        PersistedEventRecord {
            event_id: 1,
            event_hash: Some("b".repeat(64)),
            chain_root_id: "T-root".to_string(),
            chain_seq: 7,
            thread_id: "T-parent".to_string(),
            thread_seq: 2,
            event_type: event_type.to_string(),
            storage_class: "indexed".to_string(),
            ts: "2026-07-08T00:00:00Z".to_string(),
            prev_chain_event_hash: None,
            prev_thread_event_hash: None,
            payload,
        }
    }

    #[test]
    fn verify_ref_compares_hash_and_sequences() {
        let event = persisted_event("milestone", json!({}));
        let expected = EventRef {
            chain_root_id: "T-root".to_string(),
            thread_id: "T-parent".to_string(),
            chain_seq: 7,
            thread_seq: 2,
            event_hash: "b".repeat(64),
            event_type: "milestone".to_string(),
        };
        assert!(verify_ref(&event, &expected));

        let mut bad = expected;
        bad.event_hash = "c".repeat(64);
        assert!(!verify_ref(&event, &bad));
    }

    #[test]
    fn branch_anchor_requires_state_anchor_milestone() {
        let anchor = persisted_event(
            "milestone",
            json!({
                "kind": "state_anchor",
                "payload": {"label": "arc.sim_state"}
            }),
        );
        assert!(is_state_anchor(&anchor));

        let milestone = persisted_event("milestone", json!({"kind": "other", "payload": {}}));
        assert!(!is_state_anchor(&milestone));
    }

    #[test]
    fn descriptor_is_mutating_daemon_only_and_cap_gated() {
        assert!(matches!(
            DESCRIPTOR.availability,
            ServiceAvailability::DaemonOnly
        ));
        assert_eq!(
            DESCRIPTOR.required_caps,
            &["ryeos.execute.service.trace/branch"]
        );
    }
}
