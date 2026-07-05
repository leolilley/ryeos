//! `threads.cancel` — cancel a running or created thread.
//!
//! Kills the thread's process group (if any), finalizes the thread as
//! `cancelled`, and broadcasts the `thread_cancelled` event to any
//! live subscribers via `ThreadEventHub`.
//!
//! Ownership check: callers can only cancel their own threads.

use std::sync::Arc;

use anyhow::Result;
use serde_json::{json, Value};

use crate::handler_context::HandlerContext;
use crate::handler_error::HandlerError;
use crate::registry::ServiceDescriptor;
use ryeos_app::cascade::{cascade_descendants, CascadeMode};
use ryeos_app::process::{kill_by_action, resolve_shutdown_action};
use ryeos_app::state::AppState;
use ryeos_app::state_store::is_terminal_status;
use ryeos_app::thread_lifecycle::ThreadFinalizeParams;
use ryeos_engine::contracts::ThreadTerminalStatus;
use ryeos_executor::executor::ServiceAvailability;

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
        .map_err(|e| HandlerError::Internal(e.to_string()))?
        .ok_or(HandlerError::NotFound)?;

    // Ownership check: callers can only cancel their own threads.
    ctx.require_owner(thread.requested_by.as_deref())?;

    // Check if already terminal — bail early with a clear message.
    let current_status = thread.status.as_str();
    if is_terminal_status(current_status) {
        return Err(HandlerError::BadRequest(format!(
            "thread {} is already {} — cannot cancel",
            req.thread_id, current_status
        )));
    }

    // Only threads in "created" or "running" are cancellable.
    if current_status != "created" && current_status != "running" {
        return Err(HandlerError::BadRequest(format!(
            "thread {} has non-cancellable status: {}",
            req.thread_id, current_status
        )));
    }

    // If the thread has a usable PGID, kill the process group. A non-positive
    // pgid is unusable — `kill(-0, …)` would hit the daemon's own group — so it
    // is treated as "no process to kill", not signalled.
    let kill_info = if let Some(pgid) = thread.runtime.pgid.filter(|&p| p > 0) {
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
            return Err(HandlerError::Internal(format!(
                "failed to kill process group {} for thread {}: {}",
                pgid, req.thread_id, result.method
            )));
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
    //
    // Race: a graph that honours the SIGTERM cooperative-cancel flag may
    // self-finalize `cancelled` over its callback within the kill's grace window,
    // between the kill above and here. That leaves this thread already terminal,
    // so `finalize_thread` would reject the same-status transition. Treat an
    // already-terminal target as success — the cancel took effect — rather than
    // surfacing the benign race as an Internal error.
    let finalized = match state.threads.finalize_thread(&ThreadFinalizeParams {
        thread_id: req.thread_id.clone(),
        status: ThreadTerminalStatus::Cancelled.as_str().to_string(),
        outcome_code: Some(ThreadTerminalStatus::Cancelled.as_str().to_string()),
        result: None,
        error: Some(json!({
            "reason": "cancelled_by_request",
        })),
        metadata: None,
        artifacts: Vec::new(),
        final_cost: None,
        summary_json: None,
    }) {
        Ok(finalized) => finalized,
        Err(e) => {
            let current = state
                .state_store
                .get_thread(&req.thread_id)
                .map_err(|read_err| HandlerError::Internal(read_err.to_string()))?;
            match current {
                Some(thread) if is_terminal_status(&thread.status) => thread,
                _ => return Err(HandlerError::Internal(e.to_string())),
            }
        }
    };

    // `finalize_thread` persists then publishes the `thread_cancelled`
    // event, so live subscribers receive it directly.

    // Cascade to live descendants: killing this thread's own pgid does not reach
    // the children it spawned (each runs as its own setsid group), so a graph
    // blocked on an inline child would leave that child running — and authoring —
    // past the parent's cancel. Graceful, honouring each child's declared mode. A
    // walk failure is logged, not raised: the primary is already settled.
    let cascade = match cascade_descendants(
        &state.state_store,
        &req.thread_id,
        CascadeMode::Graceful,
    ) {
        Ok(report) => report,
        Err(e) => {
            tracing::warn!(
                thread_id = %req.thread_id,
                error = %e,
                "descendant cascade failed during cancel"
            );
            Vec::new()
        }
    };

    // If the cancelled thread was a followed child's chain terminal, its finalize
    // just flipped the awaiting waiter to `ready` (a degraded failure envelope) —
    // kick the parent resume live so it does not wait for the next daemon restart.
    // A no-op for a non-follow thread.
    ryeos_executor::execution::launch::kick_follow_resume_if_ready(
        &state,
        &finalized.chain_root_id,
    );
    ryeos_executor::execution::launch::kick_launch_window_for_terminal(
        &state,
        &finalized.chain_root_id,
    );

    Ok(json!({
        "thread_id": req.thread_id,
        "status": finalized.status,
        "kill": kill_info,
        "cascade": cascade,
    }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:threads/cancel",
    endpoint: "threads.cancel",
    availability: ServiceAvailability::Both,
    required_caps: &["ryeos.execute.service.threads/cancel"],
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
    fn is_terminal_detects_all_terminal_statuses() {
        for status in &[
            "completed",
            "failed",
            "cancelled",
            "killed",
            "timed_out",
            "continued",
        ] {
            assert!(
                is_terminal_status(status),
                "expected '{}' to be terminal",
                status
            );
        }
    }

    #[test]
    fn is_terminal_rejects_non_terminal() {
        for status in &["created", "running", "pending", "paused"] {
            assert!(
                !is_terminal_status(status),
                "expected '{}' to NOT be terminal",
                status
            );
        }
    }

    #[test]
    fn descriptor_has_required_caps() {
        assert_eq!(
            DESCRIPTOR.required_caps,
            &["ryeos.execute.service.threads/cancel"]
        );
    }

    #[test]
    fn descriptor_service_ref_matches_route() {
        assert_eq!(DESCRIPTOR.service_ref, "service:threads/cancel");
    }

    #[test]
    fn request_deserialize_with_caller_fields() {
        let req: Request = serde_json::from_value(json!({
            "thread_id": "T-1234",
        }))
        .unwrap();
        assert_eq!(req.thread_id, "T-1234");
    }

    #[test]
    fn request_deserialize_accepts_valid_without_caller() {
        let req: Request = serde_json::from_value(json!({"thread_id": "T-1234"})).unwrap();
        assert_eq!(req.thread_id, "T-1234");
    }
}
