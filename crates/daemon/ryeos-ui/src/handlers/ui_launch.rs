//! `ui/launch` service — consumes a launch token and sets a session cookie.
//!
//! The configured `service:ui/launch` route activates a short-lived launch token
//! (minted by the web launcher binary), establishes a browser session,
//! sets a `ryeos_session` cookie, and redirects to `/ui`.

use std::sync::Arc;

use anyhow::Result;
use serde::Deserialize;
use serde_json::{Value, json};

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

    // Activation is idempotent until token expiry so delivery loss can replay
    // the same transition without creating another successor.
    let ui_state = get_ui_state(&state).expect("UiState not set");
    let _transition = ui_state
        .lock_seat_transition()
        .map_err(|_| anyhow::anyhow!("seat transition lock poisoned"))?;
    let activation = ui_state
        .browser_sessions
        .activate_launch_token(&req.token)
        .ok_or_else(|| anyhow::anyhow!("invalid or expired launch token"))?;
    let session_id = activation.session_id;

    // Return session info. The route layer handles Set-Cookie + redirect.
    Ok(json!({
        "session_id": session_id,
        "redirect": "/ui",
        "cookie": session_cookie(&session_id, None),
    }))
}

/// The session cookie envelope the route layer turns into `Set-Cookie`.
/// Strict: the launch redirect is same-origin, so nothing legitimate needs
/// this cookie on a cross-site request.
fn session_cookie(value: &str, max_age: Option<u64>) -> Value {
    let mut cookie = json!({
        "name": "ryeos_session",
        "value": value,
        "http_only": true,
        "same_site": "Strict",
        "secure": false,
        "path": "/ui"
    });
    if let Some(max_age) = max_age {
        cookie["max_age"] = max_age.into();
    }
    cookie
}

/// The cookie envelope that makes the browser drop the session cookie.
pub(crate) fn cleared_session_cookie() -> Value {
    session_cookie("", Some(0))
}
