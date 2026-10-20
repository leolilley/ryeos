//! `ui.session.current` — return the authenticated session's context.
//!
//! The browser calls this once on load to discover the immutable authored
//! root attachment, exact presentation memberships, the currently admitted
//! binding descriptors, and the events URL. Descriptor order carries no
//! authority.
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

    let ui = get_ui_state(&state).expect("UiState not set");
    let expected = ui
        .browser_sessions
        .presentation_memberships(&session_id)
        .map_err(|error| HandlerError::Forbidden(error.to_string()))?;
    // The projection is taken while the store holds exactly the membership
    // inventory read above; a membership that changed in between refuses and
    // the browser retries, so a descriptor is never mixed from two states.
    let projected = ui
        .browser_sessions
        .with_exact_presentation_read_snapshot(
            &session_id,
            &expected,
            state.node_policy.generation_digest(),
            |locked| project_current_session(&state, locked),
        )
        .map_err(|error| HandlerError::Forbidden(error.to_string()))??;
    serde_json::to_value(projected).map_err(Into::into)
}

/// Pure projection: no store lookup, seat mutation or lock acquisition.
pub(crate) fn project_current_session(
    state: &AppState,
    session: &crate::browser_session::BrowserSession,
) -> Result<ryeos_client_base::ui::BrowserSession> {
    let bounds = super::ui_launch_mint::binding_request_bounds(state)?;

    // Private retained attachments outlive closing a Surface for history and
    // idempotent admission checks. Bootstrap exposes only live memberships and
    // the immutable launch root, not those historical descriptors as current
    // choices. Hidden transfer batches are not session attachments yet.
    let current_bindings = std::iter::once(session.root_attachment_id.as_str())
        .chain(session.presentations.values().flat_map(|membership| {
            std::iter::once(membership.root_coordinate.binding_attachment_id.as_str())
                .chain(
                    membership
                        .activated_transfer_reservations
                        .values()
                        .flat_map(|reservation| {
                            reservation
                                .current_coordinates()
                                .map(|coordinate| coordinate.binding_attachment_id.as_str())
                        }),
                )
                .chain(
                    membership
                        .surfaces
                        .values()
                        .filter(|surface| !surface.closed)
                        .flat_map(|surface| {
                            std::iter::once(surface.coordinate.binding_attachment_id.as_str())
                                .chain(
                                    surface
                                        .saved_resume
                                        .iter()
                                        .flat_map(|receipt| receipt.coordinates())
                                        .map(|coordinate| {
                                            coordinate.binding_attachment_id.as_str()
                                        }),
                                )
                        }),
                )
                .chain(
                    membership
                        .surfaces
                        .values()
                        .flat_map(|surface| surface.tab_openings.values())
                        .flat_map(|opening| opening.current_coordinates.iter())
                        .map(|coordinate| coordinate.binding_attachment_id.as_str()),
                )
        }))
        .collect::<std::collections::BTreeSet<_>>();
    let mut binding_attachments = session
        .attachments
        .values()
        .filter(|attachment| current_bindings.contains(attachment.binding_attachment_id.as_str()))
        .map(|attachment| attachment.public_descriptor(bounds))
        .collect::<Vec<_>>();
    binding_attachments
        .sort_by(|left, right| left.binding_attachment_id.cmp(&right.binding_attachment_id));
    let mut presentations = session
        .presentations
        .values()
        .map(|membership| membership.public_descriptor())
        .collect::<Vec<_>>();
    presentations.sort_by(|left, right| left.presentation_id.cmp(&right.presentation_id));
    let response = ryeos_client_base::ui::BrowserSession {
        ui_binding_contract_revision: crate::UI_BINDING_CONTRACT_REVISION.to_string(),
        session_id: session.session_id.clone(),
        root_attachment_id: session.root_attachment_id.clone(),
        presentations,
        binding_attachments,
        user_principal_id: session.user_principal_id.clone(),
        events_url: Some(format!("/ui/events/session/{}", session.session_id)),
    };

    enforce_bootstrap_bound(state, &response)?;
    Ok(response)
}

fn enforce_bootstrap_bound(
    state: &AppState,
    response: &ryeos_client_base::ui::BrowserSession,
) -> Result<()> {
    let policy = state
        .node_policy
        .require::<ryeos_node_runtime::node_policy::sections::ui_browser_sessions::UiBrowserSessionPolicy>()?;
    validate_bootstrap_bytes(response, policy.max_session_bootstrap_bytes)
}

fn validate_bootstrap_bytes(
    response: &ryeos_client_base::ui::BrowserSession,
    maximum: u64,
) -> Result<()> {
    let envelope = serde_json::json!({"result": response});
    let bytes = serde_json::to_vec(&envelope)?;
    if u64::try_from(bytes.len())? > maximum {
        return Err(
            HandlerError::Forbidden("session bootstrap exceeds signed byte limit".into()).into(),
        );
    }
    Ok(())
}

/// Validate the complete prospective bootstrap before the session owner
/// commits. The caller holds the existing transition/store ownership; this
/// helper never reenters it.
pub fn preflight_initial_publication(
    state: &AppState,
    session: &crate::browser_session::BrowserSession,
    candidate: &crate::browser_session::PresentationMembership,
    attachments: &[Arc<crate::browser_session::AdmittedBindingAttachment>],
) -> std::result::Result<(), crate::browser_session::AttachmentStoreError> {
    let mut projected = session.clone();
    let mut membership = candidate.clone();
    // Seat ids are generated thread-id coordinates. Reserve their full wire
    // length before durable seat creation, without granting an invented seat
    // authority.
    let reserved_seat_id = ryeos_app::thread_lifecycle::new_thread_id();
    membership.thread_id = Some(reserved_seat_id.clone());
    membership.chain_root_id = Some(reserved_seat_id);
    projected
        .presentations
        .insert(membership.presentation_id.clone(), membership);
    for attachment in attachments {
        projected
            .attachments
            .insert(attachment.binding_attachment_id.clone(), attachment.clone());
    }
    project_current_session(state, &projected)
        .map(|_| ())
        .map_err(|_| crate::browser_session::AttachmentStoreError::Capacity)
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:ui/session/current",
    endpoint: "ui.session.current",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| Box::pin(async move { handle(params, ctx, state).await }),
};
