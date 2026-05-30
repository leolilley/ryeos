//! `ui.session.current` — return the authenticated session's context.
//!
//! The browser calls this once on load to discover its session_id,
//! surface_ref, project_path, read_only, and events URL. The browser
//! then calls `items.effective` for the surface and opens the SSE
//! stream at `events_url`.
//!
//! Requires `browser_session` auth (cookie). No surface resolution,
//! no cap intersection, no extra round trips.

use std::sync::Arc;

use anyhow::Result;
use serde::Serialize;
use serde_json::Value;

use ryeos_api::registry::ServiceDescriptor;
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::handler_error::HandlerError;
use ryeos_app::state::AppState;
use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_engine::engine::EffectiveItemRequest;
use ryeos_executor::executor::ServiceAvailability;

use crate::state::get_ui_state;

#[derive(Debug, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Response {
    pub session_id: String,
    pub surface_ref: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effective_surface: Option<Value>,
    pub project_path: Option<String>,
    pub read_only: bool,
    pub events_url: String,
}

/// Extract session_id from the handler context's fingerprint.
/// The browser_session invoker sets `id` to `session:<session_id>`.
fn session_id_from_context(ctx: &HandlerContext) -> Option<String> {
    if ctx.fingerprint.starts_with("session:") {
        Some(
            ctx.fingerprint
                .strip_prefix("session:")
                .unwrap()
                .to_string(),
        )
    } else {
        None
    }
}

pub async fn handle(_params: Value, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    let session_id = session_id_from_context(&ctx)
        .ok_or_else(|| HandlerError::Forbidden("no browser session".into()))?;

    let session = get_ui_state(&state)
        .expect("UiState not set")
        .browser_sessions
        .get_session(&session_id)
        .ok_or(HandlerError::Forbidden("session expired or invalid".into()))?;
    let surface_ref = session.surface_ref.clone();
    let project_path = session.project_root.clone();
    let effective_surface = CanonicalRef::parse(&surface_ref)
        .ok()
        .and_then(|item_ref| {
            state
                .engine
                .effective_item(EffectiveItemRequest {
                    item_ref,
                    expected_kind: Some("surface".to_string()),
                    project_root: project_path.clone().map(Into::into),
                })
                .ok()
        })
        .map(|effective| effective.composed_value);

    let response = Response {
        session_id: session.session_id.clone(),
        surface_ref,
        effective_surface,
        project_path,
        read_only: session.read_only,
        events_url: format!("/ui/events/session/{}", session.session_id),
    };

    serde_json::to_value(response).map_err(Into::into)
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:ui/session/current",
    endpoint: "ui.session.current",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| Box::pin(async move { handle(params, ctx, state).await }),
};
