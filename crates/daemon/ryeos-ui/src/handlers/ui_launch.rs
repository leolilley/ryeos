//! `ui/launch` service — consumes a launch token and sets a session cookie.
//!
//! The configured `service:ui/launch` route consumes a single-use launch token
//! (minted by the web launcher binary), establishes a browser session,
//! sets a `ryeos_session` cookie, and redirects to `/ui`.

use std::sync::Arc;

use anyhow::Result;
use serde::Deserialize;
use serde_json::{json, Value};

use ryeos_app::handler_context::HandlerContext;
use ryeos_app::state::AppState;

use ryeos_api::registry::ServiceAvailability;

use crate::state::get_ui_state;

#[derive(Debug, Deserialize)]
pub struct Request {
    pub token: String,
}

pub const DESCRIPTOR: ryeos_api::registry::ServiceDescriptor =
    ryeos_api::registry::ServiceDescriptor {
        service_ref: "service:ui/launch",
        endpoint: "ui.launch",
        availability: ServiceAvailability::DaemonOnly,
        required_caps: &[],
        handler: |params, ctx, state| Box::pin(async move { handle(params, ctx, state).await }),
    };

pub async fn handle(input: Value, _ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    let req: Request = serde_json::from_value(input)
        .map_err(|e| anyhow::anyhow!("invalid ui.launch request: {e}"))?;

    // Consume the launch token (one-shot).
    let session_id = get_ui_state(&state)
        .expect("UiState not set")
        .browser_sessions
        .consume_launch_token(&req.token)
        .ok_or_else(|| anyhow::anyhow!("invalid or expired launch token"))?;

    // Return session info. The route layer handles Set-Cookie + redirect.
    Ok(json!({
        "session_id": session_id,
        "redirect": "/ui",
        "cookie": {
            "name": "ryeos_session",
            "value": session_id,
            "http_only": true,
            // Strict: the launch redirect is same-origin, so nothing
            // legitimate needs this cookie on a cross-site request.
            "same_site": "Strict",
            "secure": false,
            "path": "/ui"
        }
    }))
}
