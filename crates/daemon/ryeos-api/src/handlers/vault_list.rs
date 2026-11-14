//! `vault/list` — list secret key names from the node's vault.
//!
//! Returns names only, never values, and only the names the caller may
//! receive or write under the node's `sealed_vault_names` policy.

use std::sync::Arc;

use serde_json::Value;

use crate::handler_context::HandlerContext;
use crate::handler_error::{HandlerError, HandlerResult};
use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {}

pub async fn handle(
    _req: Request,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> HandlerResult<Value> {
    ctx.require_verified()?;

    let keys = state
        .vault
        .list_keys()
        .map_err(|e| HandlerError::Internal(format!("{e:#}")))?;

    // Only names this caller may receive or write are shown; the rest of the
    // shared store is not disclosed to it.
    let keys: Vec<String> = keys
        .into_iter()
        .filter(|name| {
            state
                .vault_ceilings
                .is_visible(name, &ctx.fingerprint, &ctx.scopes)
        })
        .collect();

    Ok(serde_json::json!({
        "secrets": keys,
    }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:vault/list",
    endpoint: "vault.list",
    availability: ServiceAvailability::Both,
    required_caps: &["ryeos.execute.service.vault/list"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, ctx, state).await.map_err(Into::into)
        })
    },
};
