//! `threads.cancel` — cancel a running or created thread.
//!
//! Kills the thread's process group (if any), finalizes the thread as
//! `cancelled`, and broadcasts the `thread_cancelled` event to any
//! live subscribers via `ThreadEventHub`.

use std::sync::Arc;

use anyhow::{bail, Result};
use serde_json::{json, Value};

use crate::process::{kill_by_action, resolve_shutdown_action};
use crate::service_executor::ServiceAvailability;
use crate::service_registry::ServiceDescriptor;
use crate::services::thread_lifecycle::ThreadFinalizeParams;
use crate::state::AppState;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub thread_id: String,
}

pub async fn handle(req: Request, state: Arc<AppState>) -> Result<Value> {
    let thread = state
        .state_store
        .get_thread(&req.thread_id)?
        .ok_or_else(|| anyhow::anyhow!("thread not found: {}", req.thread_id))?;

    // Check if already terminal — bail early with a clear message.
    let current_status = thread.status.as_str();
    if is_terminal(current_status) {
        bail!(
            "thread {} is already {} — cannot cancel",
            req.thread_id,
            current_status
        );
    }

    // Only threads in "created" or "running" are cancellable.
    if current_status != "created" && current_status != "running" {
        bail!(
            "thread {} has non-cancellable status: {}",
            req.thread_id,
            current_status
        );
    }

    // If the thread has a PGID, kill the process group.
    let kill_info = if let Some(pgid) = thread.runtime.pgid {
        let action = resolve_shutdown_action(
            thread
                .runtime
                .launch_metadata
                .as_ref()
                .and_then(|lm| lm.cancellation_mode),
        );
        let result = kill_by_action(pgid, action);

        // If the kill genuinely failed (not already_dead), don't
        // finalize — the process is still alive and marking it
        // cancelled would be a lie.
        if !result.success {
            bail!(
                "failed to kill process group {} for thread {}: {}",
                pgid,
                req.thread_id,
                result.method
            );
        }

        json!({
            "pgid": pgid,
            "action": format!("{:?}", action),
            "method": result.method,
            "success": result.success,
        })
    } else {
        // No PGID — thread was never attached (still in "created").
        json!({"pgid": null, "note": "no_process_to_kill"})
    };

    // Finalize via ThreadLifecycleService so scheduler fire records
    // get updated correctly (a raw state_store call would skip that).
    let finalized = state.threads.finalize_thread(&ThreadFinalizeParams {
        thread_id: req.thread_id.clone(),
        status: "cancelled".to_string(),
        outcome_code: Some("cancelled".to_string()),
        result: None,
        error: Some(json!({
            "reason": "cancelled_by_request",
        })),
        metadata: None,
        artifacts: Vec::new(),
        final_cost: None,
        summary_json: None,
    })?;

    // Broadcast the terminal event to any live subscribers for this
    // thread. The event is already persisted in the event store by
    // finalize_thread above; this just reduces latency for connected
    // clients.
    //
    // Note: finalize_thread (ThreadLifecycleService) calls
    // state_store.finalize_thread which returns PersistedEventRecords,
    // but the lifecycle service doesn't forward them to ThreadEventHub.
    // We re-fetch and publish here for latency. Subscribers who miss
    // the live broadcast will see the event on next replay.
    if let Ok(events) = state.events.replay(&crate::services::event_store::EventReplayParams {
        chain_root_id: None,
        thread_id: Some(req.thread_id.clone()),
        after_chain_seq: None,
        limit: 1,
    }) {
        for event in &events.events {
            if event.event_type == "thread_cancelled" {
                state.event_streams.publish(&req.thread_id, event.clone());
            }
        }
    }

    Ok(json!({
        "thread_id": req.thread_id,
        "status": finalized.status,
        "kill": kill_info,
    }))
}

fn is_terminal(status: &str) -> bool {
    matches!(
        status,
        "completed"
            | "failed"
            | "cancelled"
            | "killed"
            | "timed_out"
            | "continued"
    )
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:threads/cancel",
    endpoint: "threads.cancel",
    availability: ServiceAvailability::Both,
    required_caps: &["ryeos.execute.service.threads/cancel"],
    handler: |params, state| {
        Box::pin(async move {
            let req: Request = serde_json::from_value(params)?;
            handle(req, state).await
        })
    },
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_terminal_detects_all_terminal_statuses() {
        for status in &["completed", "failed", "cancelled", "killed", "timed_out", "continued"] {
            assert!(is_terminal(status), "expected '{}' to be terminal", status);
        }
    }

    #[test]
    fn is_terminal_rejects_non_terminal() {
        for status in &["created", "running", "pending", "paused"] {
            assert!(!is_terminal(status), "expected '{}' to NOT be terminal", status);
        }
    }

    #[test]
    fn descriptor_has_required_caps() {
        assert_eq!(DESCRIPTOR.required_caps, &["ryeos.execute.service.threads/cancel"]);
    }

    #[test]
    fn descriptor_service_ref_matches_route() {
        assert_eq!(DESCRIPTOR.service_ref, "service:threads/cancel");
    }

    #[test]
    fn request_deserialize_requires_thread_id() {
        let req: Result<Request, _> = serde_json::from_value(json!({}));
        assert!(req.is_err(), "empty object should fail with deny_unknown_fields");
    }

    #[test]
    fn request_deserialize_rejects_extra_fields() {
        let req: Result<Request, _> = serde_json::from_value(json!({
            "thread_id": "T-1234",
            "extra": "nope"
        }));
        assert!(req.is_err(), "extra fields should be rejected");
    }

    #[test]
    fn request_deserialize_accepts_valid() {
        let req: Request = serde_json::from_value(json!({"thread_id": "T-1234"})).unwrap();
        assert_eq!(req.thread_id, "T-1234");
    }
}
