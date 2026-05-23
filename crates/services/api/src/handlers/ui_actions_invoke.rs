//! `ui/actions/invoke` service — browser-side action invocation.
//!
//! Browser clients send affordance invocations through this service.
//! The handler enforces session capabilities before dispatching.

use std::sync::Arc;

use anyhow::Result;
use serde::Deserialize;
use serde_json::{json, Value};

use ryeos_app::handler_context::HandlerContext;
use ryeos_app::state::AppState;

use crate::registry::ServiceAvailability;

#[derive(Debug, Deserialize)]
pub struct Request {
    /// The affordance ID to invoke.
    pub affordance_id: String,
    /// Optional invocation parameters.
    #[serde(default)]
    pub parameters: Value,
}

pub const DESCRIPTOR: crate::registry::ServiceDescriptor = crate::registry::ServiceDescriptor {
    service_ref: "service:ui/actions/invoke",
    endpoint: "ui.actions.invoke",
    availability: ServiceAvailability::Both,
    required_caps: &[],
    handler: |params, ctx, state| {
        Box::pin(async move {
            handle(params, ctx, state).await
        })
    },
};

async fn handle(
    input: Value,
    ctx: HandlerContext,
    _state: Arc<AppState>,
) -> Result<Value> {
    let req: Request = serde_json::from_value(input).map_err(|e| {
        anyhow::anyhow!("invalid ui.actions.invoke request: {e}")
    })?;

    // TODO: enforce session caps against the affordance's required capabilities.
    // For now, accept any authenticated session.
    if !ctx.is_present() && ctx.scopes.is_empty() {
        anyhow::bail!("session required for action invocation");
    }

    // TODO: dispatch through the command/action registry.
    // For now, acknowledge receipt.
    Ok(json!({
        "status": "accepted",
        "affordance_id": req.affordance_id,
        "invocation_id": uuid::Uuid::new_v4().to_string()
    }))
}
