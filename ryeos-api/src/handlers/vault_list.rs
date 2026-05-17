//! `vault/list` — list secret key names from the node's vault.
//!
//! Returns names only, never values.

use std::sync::Arc;

use serde_json::Value;

use ryeos_executor::executor::ServiceAvailability;
use crate::registry::ServiceDescriptor;
use crate::handler_error::{HandlerError, HandlerResult};
use ryeos_app::state::AppState;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    #[serde(default)]
    pub _caller_fingerprint: String,
    #[serde(default)]
    pub _caller_scopes: Vec<String>,
}

pub async fn handle(req: Request, state: Arc<AppState>) -> HandlerResult<Value> {
    let keys = state
        .vault
        .list_keys(&req._caller_fingerprint)
        .map_err(|e| HandlerError::Internal(e.to_string()))?;

    Ok(serde_json::json!({
        "secrets": keys,
    }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:vault/list",
    endpoint: "vault.list",
    availability: ServiceAvailability::Both,
    required_caps: &["ryeos.execute.service.vault/list"],
    handler: |params, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, state).await.map_err(Into::into)
        })
    },
};
