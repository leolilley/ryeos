//! `commands.get` — read one command record by id.
//!
//! Ownership: a caller may only read a command belonging to a thread they own.
//! An absent command and a non-owned command both surface as `NotFound`, so the
//! endpoint never leaks the existence of another owner's command (mirroring
//! `commands.submit` / `threads.cancel`).

use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use crate::handler_context::HandlerContext;
use crate::handler_error::HandlerError;
use crate::registry::ServiceDescriptor;
use ryeos_app::runtime_db::CommandRecord;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub command_id: i64,
}

pub async fn handle(
    req: Request,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value, HandlerError> {
    let record = load_owned_command(&state, &ctx, req.command_id)?;
    serde_json::to_value(record).map_err(|e| HandlerError::Internal(e.to_string()))
}

/// Load a command by id and enforce that the caller owns its thread. Shared with
/// `commands.wait`. Returns `NotFound` both when the command is absent and when
/// the caller is not the owner — no existence leak.
pub(crate) fn load_owned_command(
    state: &AppState,
    ctx: &HandlerContext,
    command_id: i64,
) -> Result<CommandRecord, HandlerError> {
    let record = state
        .state_store
        .get_command(command_id)
        .map_err(|e| HandlerError::Internal(e.to_string()))?
        .ok_or(HandlerError::NotFound)?;
    let thread = state
        .state_store
        .get_thread(&record.thread_id)
        .map_err(|e| HandlerError::Internal(e.to_string()))?
        .ok_or(HandlerError::NotFound)?;
    ctx.require_owner(thread.requested_by.as_deref())?;
    Ok(record)
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:commands/get",
    endpoint: "commands.get",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.commands/get"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, ctx, state).await.map_err(Into::into)
        })
    },
};

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_requires_command_id() {
        let ok: Request = serde_json::from_value(json!({"command_id": 5})).unwrap();
        assert_eq!(ok.command_id, 5);
        // deny_unknown_fields rejects extras.
        assert!(serde_json::from_value::<Request>(json!({"command_id": 5, "x": 1})).is_err());
        // command_id is required.
        assert!(serde_json::from_value::<Request>(json!({})).is_err());
    }

    #[test]
    fn descriptor_declares_its_cap_and_route() {
        assert_eq!(DESCRIPTOR.endpoint, "commands.get");
        assert_eq!(DESCRIPTOR.service_ref, "service:commands/get");
        assert_eq!(
            DESCRIPTOR.required_caps,
            &["ryeos.execute.service.commands/get"]
        );
    }
}
