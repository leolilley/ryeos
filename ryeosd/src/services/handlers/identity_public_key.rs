//! `identity.public_key` — return the daemon's public identity document.

use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use crate::service_executor::ServiceAvailability;
use crate::service_registry::ServiceDescriptor;
use crate::state::AppState;

#[derive(serde::Deserialize, Default)]
#[serde(default)]
pub struct Request {}

pub async fn handle(_req: Request, state: Arc<AppState>) -> Result<Value> {
    let identity_path = state
        .config
        .state_dir
        .join(".ai")
        .join("identity")
        .join("public-identity.json");
    let doc = crate::identity::NodeIdentity::load_public_identity(&identity_path)?;
    serde_json::to_value(doc).map_err(Into::into)
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:identity/public_key",
    endpoint: "identity.public_key",
    availability: ServiceAvailability::Both,
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
