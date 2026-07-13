//! `commands.submit` — enqueue a command for an existing thread.
//!
//! The caller identity is derived from the injected `HandlerContext`,
//! not from a client-supplied `requested_by` field. This prevents
//! caller-forgery: only cryptographically verified callers get their
//! fingerprint recorded as `requested_by`.
//!
//! Ownership check: callers can only submit control commands (cancel /
//! kill / interrupt / continue) to threads they own. This mirrors the
//! read path (`events/replay`, chain-tail) and the other control path
//! (`threads/cancel`) — holding the capability is not enough; the caller
//! must be the thread's owner.

use std::sync::Arc;

use serde_json::Value;

use crate::handler_context::HandlerContext;
use crate::handler_error::HandlerError;
use crate::registry::ServiceDescriptor;
use ryeos_app::cascade::{stop_thread_and_descendants, CascadeMode};
use ryeos_app::command_service::CommandSubmitParams;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub thread_id: String,
    pub command_type: String,
    #[serde(default)]
    pub params: Option<Value>,
}

pub async fn handle(
    req: Request,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value, HandlerError> {
    // Ownership check: callers can only control their own threads. The
    // service layer also loads the thread for its state-machine guard;
    // we load here to keep authz in the API layer where the verified
    // caller context lives.
    let thread = state
        .state_store
        .get_thread(&req.thread_id)
        .map_err(|e| HandlerError::Internal(e.to_string()))?
        .ok_or(HandlerError::NotFound)?;
    ctx.require_owner(thread.requested_by.as_deref())?;

    // Derive requested_by from verified caller context.
    // Only cryptographically verified callers get recorded.
    let requested_by = if ctx.is_present() {
        Some(ctx.fingerprint.clone())
    } else {
        None
    };

    let thread_id = req.thread_id.clone();
    let command_type = req.command_type.clone();
    let record = state
        .commands
        .submit(&CommandSubmitParams {
            thread_id: req.thread_id,
            command_type: req.command_type,
            requested_by,
            params: req.params,
        })
        .map_err(|e| HandlerError::Internal(e.to_string()))?;

    // Daemon-side immediate enforcement: a cancel/kill signals the target AND
    // cascades to its live descendants right now, regardless of whether the
    // target cooperatively handles its own command (a directive has no drain; a
    // graph blocked on an inline child cannot claim between nodes). kill hard-
    // SIGKILLs; cancel sends a graceful SIGTERM, which both runtimes now honour
    // as a cooperative cancel. A failure here is logged, not raised — the command
    // is already durably enqueued.
    let stop_mode = match command_type.as_str() {
        "kill" => Some(CascadeMode::Hard),
        "cancel" => Some(CascadeMode::Graceful),
        _ => None,
    };
    if let Some(mode) = stop_mode {
        match stop_thread_and_descendants(&state, &thread_id, mode) {
            Ok(report) => tracing::info!(
                thread_id = %thread_id,
                command_type = %command_type,
                report = %report,
                "cancel/kill signalled target and descendants"
            ),
            Err(e) => tracing::warn!(
                thread_id = %thread_id,
                command_type = %command_type,
                error = %e,
                "cancel/kill stop failed on command submit"
            ),
        }
    }

    serde_json::to_value(record).map_err(|e| HandlerError::Internal(e.to_string()))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:commands/submit",
    endpoint: "commands.submit",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.commands/submit"],
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
    fn request_deserializes_without_requested_by() {
        let req: Request = serde_json::from_value(serde_json::json!({
            "thread_id": "t:123",
            "command_type": "cancel",
        }))
        .unwrap();
        assert_eq!(req.thread_id, "t:123");
    }

    #[test]
    fn spoofed_requested_by_is_rejected() {
        // requested_by is no longer a field on Request —
        // serde(deny_unknown_fields) must reject it.
        let result = serde_json::from_value::<Request>(serde_json::json!({
            "thread_id": "t:123",
            "command_type": "cancel",
            "requested_by": "fp:attacker",
        }));
        assert!(
            result.is_err(),
            "spoofed requested_by should be rejected by deny_unknown_fields"
        );
    }

    #[test]
    fn require_owner_blocks_non_owner_as_not_found() {
        // The ownership guard the handler applies before submitting:
        // a verified caller whose fingerprint differs from the thread's
        // owner gets NotFound (never 403 — no existence leak).
        let ctx = HandlerContext::new("fp:attacker".to_string(), Vec::new(), true);
        let err = ctx
            .require_owner(Some("fp:owner"))
            .expect_err("non-owner must be rejected");
        assert!(
            matches!(err, HandlerError::NotFound),
            "non-owner should get NotFound, got {err:?}"
        );
    }

    #[test]
    fn require_owner_allows_matching_owner() {
        let ctx = HandlerContext::new("fp:owner".to_string(), Vec::new(), true);
        assert!(ctx.require_owner(Some("fp:owner")).is_ok());
    }

    #[test]
    fn require_owner_rejects_unverified_caller() {
        // Even a fingerprint match must fail when the caller is not
        // cryptographically verified (fail-closed).
        let ctx = HandlerContext::new("fp:owner".to_string(), Vec::new(), false);
        assert!(ctx.require_owner(Some("fp:owner")).is_err());
    }
}
