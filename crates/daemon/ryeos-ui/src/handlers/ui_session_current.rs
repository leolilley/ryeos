//! `ui.session.current` — return the authenticated session's context.
//!
//! The browser calls this once on load to discover the immutable authored
//! surface attachment, every currently admitted binding descriptor, and the
//! events URL. Descriptor order carries no authority.
//!
//! Requires `browser_session` auth (cookie). No cap intersection, no
//! extra round trips.

use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use ryeos_api::registry::ServiceDescriptor;
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::handler_error::HandlerError;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

use crate::state::get_ui_state;

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

    let mut binding_attachments = session
        .attachments
        .values()
        .map(
            |attachment| ryeos_client_base::ui::binding::UiBindingAttachment {
                binding_attachment_id: attachment.binding_attachment_id.clone(),
                binding_generation: attachment.binding_generation,
                binding_digest: attachment.compiled_binding.binding_digest.clone(),
                surface_ref: attachment.surface_ref.clone(),
                surface_generation: attachment
                    .compiled_binding
                    .binding
                    .surface
                    .effective_definition_digest
                    .as_str()
                    .to_owned(),
                effective_surface: attachment.effective_surface.clone(),
                project_path: attachment.project_query_identity.clone(),
                posture: match attachment.compiled_binding.posture {
                    crate::compiled_binding::EffectiveUiPosture::ObservationOnly => {
                        ryeos_client_base::ui::binding::UiEffectivePosture::ObservationOnly
                    }
                    crate::compiled_binding::EffectiveUiPosture::Interactive => {
                        ryeos_client_base::ui::binding::UiEffectivePosture::Interactive
                    }
                },
                binding_request_bounds: ryeos_client_base::ui::UiBindingRequestBounds {
                    max_request_bytes: route_max,
                    max_input_bytes: route_max,
                },
            },
        )
        .collect::<Vec<_>>();
    binding_attachments
        .sort_by(|left, right| left.binding_attachment_id.cmp(&right.binding_attachment_id));
    let response = ryeos_client_base::ui::BrowserSession {
        ui_binding_contract_revision: crate::UI_BINDING_CONTRACT_REVISION.to_string(),
        session_id: session.session_id.clone(),
        surface_attachment_id: session.surface_attachment_id.clone(),
        binding_attachments,
        user_principal_id: session.user_principal_id.clone(),
        events_url: Some(format!("/ui/events/session/{}", session.session_id)),
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
