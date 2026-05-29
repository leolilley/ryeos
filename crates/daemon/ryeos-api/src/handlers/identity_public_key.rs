//! `identity/public_key` — returns the daemon's public identity document.
//!
//! Used by the CLI to discover the daemon's `principal_id` for
//! audience binding in cross-machine signed requests. This route
//! is served as `auth: none` so unauthenticated clients can fetch it.

use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

#[derive(serde::Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Request {}

pub async fn handle(_req: Request, state: Arc<AppState>) -> Result<Value> {
    let principal_id = state.identity.principal_id();
    let fingerprint = state.identity.fingerprint().to_string();
    let site_id = state.threads.site_id().to_string();
    let mut resp = serde_json::json!({
        "principal_id": principal_id,
        "fingerprint": fingerprint,
        "site_id": site_id,
    });
    if let Some(ref vfp) = state.vault_fingerprint {
        resp["vault_fingerprint"] = Value::String(vfp.clone());
    }
    Ok(resp)
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:identity/public_key",
    endpoint: "identity.public_key",
    availability: ServiceAvailability::Both,
    required_caps: &[],
    handler: |params, _ctx, state| {
        Box::pin(async move {
            let req: Request = if params.is_null() {
                Request::default()
            } else {
                crate::handler_error::parse_request(params)?
            };
            handle(req, state).await
        })
    },
};
