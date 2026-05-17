//! `remote/configure` — add or update a remote node config.

use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use crate::remote::client::RemoteClient;
use crate::remote::config;
use ryeos_executor::executor::ServiceAvailability;
use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// Name for the remote.
    pub name: String,
    /// URL of the remote node (HTTPS required except for loopback).
    pub url: String,
}

pub async fn handle(req: Request, state: Arc<AppState>) -> Result<Value> {
    config::validate_url(&req.url)?;

    // Discover the remote's public key
    let client = RemoteClient::new(&req.url, "", state.identity.clone());
    let pubkey = client.get_public_key().await?;

    let mut remotes = config::load_remotes(&state.config.system_space_dir)?;
    remotes.insert(req.name.clone(), config::RemoteConfig {
        name: req.name.clone(),
        url: req.url.clone(),
        principal_id: pubkey.principal_id.clone(),
        vault_public_key: pubkey.vault_public_key.clone(),
    });
    config::save_remotes(&state.config.system_space_dir, &remotes)?;

    Ok(serde_json::json!({
        "configured": req.name,
        "url": req.url,
        "principal_id": pubkey.principal_id,
        "fingerprint": pubkey.fingerprint,
    }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:remote/configure",
    endpoint: "remote.configure",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.remote.configure"],
    handler: |params, state| {
        Box::pin(async move {
            let req: Request = serde_json::from_value(params)?;
            handle(req, state).await
        })
    },
};
