//! `vault/set` — set a secret on the node's vault.
//!
//! Server-side sealing: the daemon re-encrypts the store after inserting
//! the new key. TLS protects the plaintext in transit.

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
    pub value: String,
    #[serde(default)]
    pub _ctx: HandlerContext,
}

pub async fn handle(req: Request, state: Arc<AppState>) -> HandlerResult<Value> {
    // Vault writes are scoped to the caller's fingerprint — must be verified.
    req._ctx.require_verified()?;

    state
        .vault
        .set_secret(&req._ctx.fingerprint, &req.name, &req.value)
        .map_err(|e| {
            // Vault validation errors (key name, blocked names) are user
            // errors → 400. Anything else is 500.
            let msg = format!("{e:#}");
            if msg.starts_with("vault: key name") {
                HandlerError::BadRequest(msg)
            } else {
                HandlerError::Internal(msg)
            }
        })?;

    let mut response = serde_json::json!({
        "name": req.name,
    });
    if let Some(ref fp) = state.vault_fingerprint {
        response["vault_fingerprint"] = serde_json::Value::String(fp.clone());
    }

    Ok(response)
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:vault/set",
    endpoint: "vault.set",
    availability: ServiceAvailability::Both,
    required_caps: &["ryeos.execute.service.vault/set"],
    handler: |params, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, state).await.map_err(Into::into)
        })
    },
};
