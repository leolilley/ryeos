//! Explicit session-wide binding attachment lifecycle operations.

use std::sync::Arc;

use anyhow::Result;
use serde::Deserialize;
use serde_json::{Value, json};

use ryeos_api::registry::ServiceDescriptor;
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::handler_error::HandlerError;
use ryeos_app::state::AppState;
use ryeos_app::thread_lifecycle::ThreadFinalizeParams;
use ryeos_executor::executor::ServiceAvailability;

use crate::browser_session::{AttachmentStoreError, BindingAttachmentCoordinate};
use crate::state::get_ui_state;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DetachRequest {
    binding_attachment_id: String,
    binding_generation: u64,
    binding_digest: String,
}

impl DetachRequest {
    fn coordinate(&self) -> BindingAttachmentCoordinate {
        BindingAttachmentCoordinate {
            binding_attachment_id: self.binding_attachment_id.clone(),
            binding_generation: self.binding_generation,
            binding_digest: self.binding_digest.clone(),
        }
    }
}

fn session_id(ctx: &HandlerContext) -> Result<String, HandlerError> {
    ctx.fingerprint
        .strip_prefix("session:")
        .map(str::to_owned)
        .ok_or_else(|| HandlerError::Forbidden("session cookie required".into()))
}

fn client_ref(coordinate: &BindingAttachmentCoordinate) -> String {
    format!(
        "client:ryeos/ui-session/{}/{}:{}",
        coordinate.binding_attachment_id, coordinate.binding_generation, coordinate.binding_digest,
    )
}

pub async fn handle_detach(
    params: Value,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    let ui = get_ui_state(&state).expect("UiState not set");
    let _transition = ui
        .lock_seat_transition()
        .map_err(|_| HandlerError::Internal("seat transition lock poisoned".into()))?;
    let session_id = session_id(&ctx)?;
    let request: DetachRequest = serde_json::from_value(params)
        .map_err(|error| HandlerError::BadRequest(format!("invalid request: {error}")))?;
    let coordinate = request.coordinate();
    let attachment = match ui
        .browser_sessions
        .attachment_for_detach(&session_id, &coordinate)
    {
        Ok(attachment) => attachment,
        Err(AttachmentStoreError::SurfaceAttachmentImmutable) => {
            return Err(HandlerError::Structured {
                code: "surface_attachment_immutable".into(),
                status: 409,
                body: json!({"code":"surface_attachment_immutable"}),
            }
            .into());
        }
        Err(error) => return Err(HandlerError::Forbidden(error.to_string()).into()),
    };

    let Some(_attachment) = attachment else {
        return Ok(json!({"detached":false,"attachment":coordinate}));
    };
    let owner = format!("session:{session_id}");
    let exact_client_ref = client_ref(&coordinate);
    // The lease table is keyed by every seat thread and queried without a
    // presentation limit, so cleanup cannot silently omit an older seat.
    for thread_id in state
        .state_store
        .seat_leases_for_owner_client(&owner, &exact_client_ref)?
    {
        let detail = state
            .state_store
            .get_thread(&thread_id)?
            .ok_or_else(|| HandlerError::Internal("leased seat thread disappeared".into()))?;
        if detail.kind != "seat_session"
            || detail.requested_by.as_deref() != Some(owner.as_str())
            || detail.executor_ref != exact_client_ref
        {
            return Err(HandlerError::Internal(
                "attachment seat lease identity is inconsistent".into(),
            )
            .into());
        }
        if detail.status == "running" {
            state.threads.finalize_thread(&ThreadFinalizeParams {
                thread_id: detail.thread_id.clone(),
                status: "completed".into(),
                outcome_code: None,
                result: None,
                error: None,
                metadata: None,
                artifacts: Vec::new(),
                final_cost: None,
                summary_json: None,
            })?;
        }
        state.state_store.remove_seat_lease(&detail.thread_id)?;
    }
    // Revoke only after all recoverable cleanup completes. A failed cleanup
    // leaves the attachment admitted, so the exact request can be retried.
    ui.browser_sessions
        .revoke_attachment(&session_id, &coordinate)
        .map_err(|error| HandlerError::Forbidden(error.to_string()))?;
    Ok(json!({"detached":true,"attachment":coordinate}))
}

pub const DETACH_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:ui/session/attachments/detach",
    endpoint: "ui.session.attachments.detach",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| Box::pin(handle_detach(params, ctx, state)),
};
