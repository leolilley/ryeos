//! `ui.session.current` — return the authenticated session's context.
//!
//! The browser calls this once on load to discover its session_id,
//! surface_ref, project_path, derived posture, and events URL — plus the
//! effective surface with its bound views already embedded, so the
//! whole boot is this one call before opening the SSE stream at
//! `events_url`.
//!
//! Requires `browser_session` auth (cookie). No cap intersection, no
//! extra round trips.

use std::sync::Arc;

use anyhow::Result;
use serde::Serialize;
use serde_json::Value;

use ryeos_api::registry::ServiceDescriptor;
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::handler_error::HandlerError;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

use crate::compiled_binding::EffectiveUiPosture;
use crate::state::get_ui_state;

#[derive(Debug, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Response {
    pub ui_binding_contract_revision: &'static str,
    pub session_id: String,
    pub surface_ref: String,
    pub effective_surface: Value,
    pub project_path: Option<String>,
    pub binding_digest: String,
    pub posture: EffectiveUiPosture,
    pub binding_request_bounds: ryeos_client_base::ui::UiBindingRequestBounds,
    pub expires_in_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_principal_id: Option<String>,
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
    let route_max = state
        .node_config
        .routes
        .iter()
        .find(|route| {
            route.response.source.as_deref()
                == Some(super::ui_invocations_dispatch::DESCRIPTOR.service_ref)
        })
        .map(|route| route.limits.body_bytes_max)
        .ok_or_else(|| HandlerError::Internal("UI binding dispatch route is absent".into()))?;

    let response = Response {
        ui_binding_contract_revision: crate::UI_BINDING_CONTRACT_REVISION,
        session_id: session.session_id.clone(),
        surface_ref,
        effective_surface: session.effective_surface.clone(),
        project_path,
        binding_digest: session.compiled_binding.binding_digest.clone(),
        posture: session.compiled_binding.posture,
        binding_request_bounds: ryeos_client_base::ui::UiBindingRequestBounds {
            max_request_bytes: route_max,
            max_input_bytes: route_max,
        },
        expires_in_ms: u64::try_from(
            session
                .expires_at
                .saturating_duration_since(std::time::Instant::now())
                .as_millis(),
        )
        .unwrap_or(u64::MAX),
        user_principal_id: session.user_principal_id.clone(),
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
