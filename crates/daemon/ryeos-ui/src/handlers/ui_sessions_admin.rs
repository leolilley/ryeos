//! `ui.sessions.list` and `ui.sessions.revoke` — the configured local
//! operator's view of live browser sessions and the power to end one.
//!
//! Both require a signed request from the configured local operator. Neither
//! ever returns a cookie value or a launch token.

use std::sync::Arc;

use anyhow::Result;
use serde::Deserialize;
use serde_json::{Value, json};

use ryeos_api::registry::ServiceDescriptor;
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::handler_error::HandlerError;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

use crate::state::get_ui_state;

const LIST_CAP: &str = "ryeos.execute.service.ui/sessions/list";
const REVOKE_CAP: &str = "ryeos.execute.service.ui/sessions/revoke";

pub const LIST_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:ui/sessions/list",
    endpoint: "ui.sessions.list",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[LIST_CAP],
    handler: |params, ctx, state| Box::pin(async move { list(params, ctx, state).await }),
};

pub const REVOKE_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:ui/sessions/revoke",
    endpoint: "ui.sessions.revoke",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[REVOKE_CAP],
    handler: |params, ctx, state| Box::pin(async move { revoke(params, ctx, state).await }),
};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RevokeRequest {
    pub session_id: String,
}

fn require_operator(ctx: &HandlerContext, state: &AppState) -> Result<()> {
    ryeos_app::operator_authority::require_local_configured_operator(state, ctx)
        .map_err(|_| HandlerError::Forbidden("configured local operator required".into()))?;
    Ok(())
}

pub async fn list(_params: Value, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    require_operator(&ctx, &state)?;
    let ui = get_ui_state(&state).expect("UiState not set");
    Ok(json!({ "sessions": ui.browser_sessions.list_sessions() }))
}

pub async fn revoke(params: Value, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    let req: RevokeRequest = ryeos_app::handler_error::parse_request(params)?;
    require_operator(&ctx, &state)?;
    let ui = get_ui_state(&state).expect("UiState not set");
    let _transition = ui
        .lock_seat_transition()
        .map_err(|_| HandlerError::Internal("seat transition lock poisoned".into()))?;
    if !ui.browser_sessions.revoke_session(&req.session_id) {
        return Err(HandlerError::NotFound.into());
    }
    Ok(json!({ "session_id": req.session_id, "revoked": true }))
}
