//! `commands.submit` — enqueue a command for an existing thread.
//!
//! The caller identity is derived from the injected `HandlerContext`,
//! not from a client-supplied `requested_by` field. This prevents
//! caller-forgery: only cryptographically verified callers get their
//! fingerprint recorded as `requested_by`.

use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use crate::handler_context::HandlerContext;
use crate::registry::ServiceDescriptor;
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

pub async fn handle(req: Request, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    // Derive requested_by from verified caller context.
    // Only cryptographically verified callers get recorded.
    let requested_by = if ctx.is_present() {
        Some(ctx.fingerprint.clone())
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
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, ctx, state).await
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
}
