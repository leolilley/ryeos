//! `vault/delete` — delete a secret from the node's vault.
//!
//! Idempotent: deleting a missing key returns `{ deleted: false }`.

use std::sync::Arc;

use serde_json::Value;

use ryeos_executor::executor::ServiceAvailability;
use crate::registry::ServiceDescriptor;
use crate::handler_error::{HandlerError, HandlerResult};
use crate::handler_context::HandlerContext;
use ryeos_app::state::AppState;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub name: String,
    #[serde(default)]
    pub _ctx: HandlerContext,
}

pub async fn handle(req: Request, state: Arc<AppState>) -> HandlerResult<Value> {
    let deleted = state
        .vault
        .delete_secret(&req._ctx.fingerprint, &req.name)
        .map_err(|e| {
            let msg = format!("{e:#}");
            if msg.starts_with("vault: key name") {
                HandlerError::BadRequest(msg)
            } else {
                HandlerError::Internal(msg)
            }
        })?;

    Ok(serde_json::json!({
        "name": req.name,
        "deleted": deleted,
    }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:vault/delete",
    endpoint: "vault.delete",
    availability: ServiceAvailability::Both,
    required_caps: &["ryeos.execute.service.vault/delete"],
    handler: |params, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, state).await.map_err(Into::into)
        })
    },
};
