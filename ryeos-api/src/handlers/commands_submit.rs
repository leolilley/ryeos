//! `commands.submit` — enqueue a command for an existing thread.
//!
//! The caller identity is derived from the injected `HandlerContext`,
//! not from a client-supplied `requested_by` field. This prevents
//! caller-forgery: only cryptographically verified callers get their
//! fingerprint recorded as `requested_by`.

use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use ryeos_executor::executor::ServiceAvailability;
use crate::registry::ServiceDescriptor;
use crate::handler_context::HandlerContext;
use crate::handler_error::HandlerError;
use ryeos_app::command_service::CommandSubmitParams;
use ryeos_app::state::AppState;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub thread_id: String,
    pub command_type: String,
    #[serde(default)]
    pub params: Option<Value>,
    /// Injected by service_invocation / executor. Typed caller context.
    #[serde(default)]
    pub _ctx: HandlerContext,
}

pub async fn handle(req: Request, state: Arc<AppState>) -> Result<Value> {
    // Derive requested_by from verified caller context.
    // Only cryptographically verified callers get recorded.
    let requested_by = if req._ctx.is_present() {
        Some(req._ctx.fingerprint.clone())
    } else {
        None
    };

    let record = state.commands.submit(&CommandSubmitParams {
        thread_id: req.thread_id,
        command_type: req.command_type,
        requested_by,
        params: req.params,
    })?;
    serde_json::to_value(record).map_err(Into::into)
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:commands/submit",
    endpoint: "commands.submit",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.commands/submit"],
    handler: |params, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, state).await
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
        assert!(req._ctx.fingerprint.is_empty()); // no caller context injected
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
        assert!(result.is_err(), "spoofed requested_by should be rejected by deny_unknown_fields");
    }

    #[test]
    fn ctx_fingerprint_used_as_requested_by() {
        let req: Request = serde_json::from_value(serde_json::json!({
            "thread_id": "t:123",
            "command_type": "cancel",
            "_ctx": {
                "fingerprint": "fp:abc",
                "scopes": ["execute"],
                "verified": true,
            },
        }))
        .unwrap();
        // Handler would derive requested_by = Some("fp:abc")
        assert!(req._ctx.is_present());
        assert_eq!(req._ctx.fingerprint, "fp:abc");
    }

    #[test]
    fn unverified_ctx_not_used_as_requested_by() {
        let req: Request = serde_json::from_value(serde_json::json!({
            "thread_id": "t:123",
            "command_type": "cancel",
            "_ctx": {
                "fingerprint": "route:health",
                "scopes": [],
                "verified": false,
            },
        }))
        .unwrap();
        // Handler would derive requested_by = None (unverified)
        assert!(!req._ctx.is_present());
    }
}
