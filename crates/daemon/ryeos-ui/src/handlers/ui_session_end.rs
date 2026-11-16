//! `ui.session.end` — a browser session ends itself.
//!
//! Requires `browser_session` auth. The session is removed from the store and
//! the response envelope tells the route layer to clear the session cookie.

use std::sync::Arc;

use anyhow::Result;
use serde_json::{Value, json};

use ryeos_api::registry::ServiceDescriptor;
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::handler_error::HandlerError;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

use crate::state::get_ui_state;

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:ui/session/end",
    endpoint: "ui.session.end",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| Box::pin(async move { handle(params, ctx, state).await }),
};

pub async fn handle(_params: Value, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    let session_id = ctx
        .fingerprint
        .strip_prefix("session:")
        .map(str::to_owned)
        .ok_or_else(|| HandlerError::Forbidden("no browser session".into()))?;
    let ui = get_ui_state(&state).expect("UiState not set");
    let _transition = ui
        .lock_seat_transition()
        .map_err(|_| HandlerError::Internal("seat transition lock poisoned".into()))?;
    ui.browser_sessions.end_session(&session_id);
    Ok(json!({
        "session_id": session_id,
        "ended": true,
        "redirect": "/ui",
        "cookie": super::ui_launch::cleared_session_cookie(),
    }))
}
