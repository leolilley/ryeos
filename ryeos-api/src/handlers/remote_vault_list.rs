//! `remote/vault-list` — list secrets on a remote node's vault.

use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use crate::remote::client::RemoteClient;
use ryeos_executor::executor::ServiceAvailability;
use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    #[serde(default = "default_remote")]
    pub remote: String,
}

fn default_remote() -> String { "default".to_string() }

pub async fn handle(req: Request, state: Arc<AppState>) -> Result<Value> {
    let client = RemoteClient::from_named_remote(&state, &req.remote)?;
    client.vault_list().await
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:remote/vault-list",
    endpoint: "remote.vault_list",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.remote.execute"],
    handler: |params, _ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, state).await
        })
    },
};
