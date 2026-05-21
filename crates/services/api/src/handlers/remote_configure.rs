//! `remote/configure` — add or update a remote node config.

use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use crate::registry::ServiceDescriptor;
use crate::remote::client::RemoteClient;
use crate::remote::config;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// Name for the remote.
    pub remote: String,
    /// URL of the remote node (HTTPS required except for loopback).
    pub url: String,
}

pub async fn handle(req: Request, state: Arc<AppState>) -> Result<Value> {
    config::validate_url(&req.url)?;

    // Discover the remote's public key
    let client = RemoteClient::new(&req.url, "", state.identity.clone());
    let pubkey = client.get_public_key().await?;

    // Fetch the remote's ingest-ignore rules — required for push to work
    // correctly. Fail hard if unavailable.
    let ingest_ignore = client.get_ingest_ignore().await?;

    let mut remotes = config::load_remotes(&state.config.system_space_dir)?;
    let vault_fp = pubkey.vault_fingerprint.clone();
    remotes.insert(
        req.remote.clone(),
        config::RemoteConfig {
            name: req.remote.clone(),
            url: req.url.clone(),
            principal_id: pubkey.principal_id.clone(),
            vault_fingerprint: pubkey.vault_fingerprint,
            ingest_ignore,
            project_bindings: remotes
                .get(&req.remote)
                .map(|r| r.project_bindings.clone())
                .unwrap_or_default(),
        },
    );
    config::save_remotes(&state.config.system_space_dir, &remotes)?;

    Ok(serde_json::json!({
        "configured": req.remote,
        "url": req.url,
        "principal_id": pubkey.principal_id,
        "fingerprint": pubkey.fingerprint,
        "vault_fingerprint": vault_fp,
    }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:remote/configure",
    endpoint: "remote.configure",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.remote.configure"],
    handler: |params, _ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, state).await
        })
    },
};
