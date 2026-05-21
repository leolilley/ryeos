//! `remote/authorize` — local CLI wrapper to authorize a key on a remote node.

use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use crate::registry::ServiceDescriptor;
use crate::remote::client::RemoteClient;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// Remote name (default: "default").
    #[serde(default = "default_remote")]
    pub remote: String,
    /// Ed25519 public key in "ed25519:<base64>" format.
    pub public_key: String,
    /// Human-readable label.
    pub label: String,
    /// Capabilities to grant. Accepts a single string or an array
    /// (see `scalar_or_vec`); the CLI's heuristic binder emits a
    /// scalar for a single `--scopes <X>` invocation.
    #[serde(deserialize_with = "ryeos_runtime::scalar_or_vec::deserialize")]
    pub scopes: Vec<String>,
}

fn default_remote() -> String {
    "default".to_string()
}

pub async fn handle(req: Request, state: Arc<AppState>) -> Result<Value> {
    let client = RemoteClient::from_named_remote(&state, &req.remote)?;
    let resp = client
        .authorize_key(&req.public_key, &req.label, &req.scopes)
        .await?;
    serde_json::to_value(resp).map_err(Into::into)
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:remote/authorize",
    endpoint: "remote.authorize",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.remote.admin"],
    handler: |params, _ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, state).await
        })
    },
};
