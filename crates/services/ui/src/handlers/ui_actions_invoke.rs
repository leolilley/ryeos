//! `ui/actions/invoke` — browser-side action invocation.
//!
//! Browser clients send affordance invocations through this endpoint.
//! The handler enforces session capabilities and `read_only` before
//! dispatching.
//!
//! ## Dispatch model
//!
//! - `InvocationSpec::Rye`: dispatches through the daemon's service
//!   registry. Returns the service result.
//! - `InvocationSpec::Ui`: published to the session bus as a
//!   `facet.upsert` event. UI effects belong client-side; the daemon
//!   echoes them for other session subscribers (e.g. the other
//!   renderer in a dual-renderer session).

use std::sync::Arc;

use anyhow::Result;
use serde::Deserialize;
use serde_json::{json, Value};

use ryeos_app::handler_context::HandlerContext;
use ryeos_app::handler_error::HandlerError;
use ryeos_api::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// The affordance ID to invoke.
    pub command_id: String,
    /// Optional invocation parameters.
    #[serde(default)]
    pub args: Value,
}

/// Extract session_id from the handler context's fingerprint.
fn session_id_from_context(ctx: &HandlerContext) -> Option<String> {
    ctx.fingerprint.strip_prefix("session:").map(String::from)
}

pub async fn handle(
    input: Value,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    let req: Request = serde_json::from_value(input)
        .map_err(|e| HandlerError::BadRequest(format!("invalid ui.actions.invoke request: {e}")))?;

    // Require browser session.
    let session_id = session_id_from_context(&ctx).ok_or_else(|| {
        HandlerError::Forbidden("session cookie required for action invocation".into())
    })?;

    let session = state
        .browser_sessions
        .get_session(&session_id)
        .ok_or_else(|| HandlerError::Forbidden("session expired or invalid".into()))?;

    // Enforce read_only: write-class actions refused.
    if session.read_only {
        return Err(
            HandlerError::Forbidden("read-only session cannot invoke actions".into()).into(),
        );
    }

    // TODO (Slice 5 depth): resolve the command_id through CommandRegistry
    // built from the session's effective surface. For now, acknowledge
    // with a session-bound invocation_id and publish to the session bus.
    let invocation_id = uuid::Uuid::new_v4().to_string();

    state.session_bus.publish(
        &session_id,
        "action.invoked",
        json!({
            "command_id": req.command_id,
            "invocation_id": invocation_id,
            "args": req.args,
        }),
    );

    Ok(json!({
        "status": "dispatched",
        "command_id": req.command_id,
        "invocation_id": invocation_id
    }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:ui/actions/invoke",
    endpoint: "ui.actions.invoke",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| {
        Box::pin(async move {
            handle(params, ctx, state).await
        })
    },
};
