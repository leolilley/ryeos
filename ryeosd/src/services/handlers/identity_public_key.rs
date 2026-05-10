//! `identity/public_key` — returns the daemon's public identity document.
//!
//! Used by the CLI to discover the daemon's `principal_id` for
//! audience binding in cross-machine signed requests. This route
//! is served as `auth: none` so unauthenticated clients can fetch it.

use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use crate::service_executor::ServiceAvailability;
use crate::service_registry::ServiceDescriptor;
use crate::state::AppState;

#[derive(serde::Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Request {}

pub async fn handle(_req: Request, state: Arc<AppState>) -> Result<Value> {
    let principal_id = state.identity.principal_id();
    let fingerprint = state.identity.fingerprint().to_string();
    Ok(serde_json::json!({
        "principal_id": principal_id,
        "fingerprint": fingerprint,
    }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:identity/public_key",
    endpoint: "identity.public_key",
    availability: ServiceAvailability::Both,
    required_caps: &[],
    handler: |params, state| {
        Box::pin(async move {
            let req: Request = if params.is_null() {
                Request::default()
            } else {
                serde_json::from_value(params)?
            };
            handle(req, state).await
        })
    },
};
