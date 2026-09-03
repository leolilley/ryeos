//! `remote/admit` — claim a one-time admission token on a configured remote.

use std::sync::Arc;

use anyhow::Result;
use base64::Engine as _;
use serde_json::Value;

use crate::registry::ServiceDescriptor;
use crate::remote::client::RemoteClient;
use crate::remote::config;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// Remote name (default: "default").
    #[serde(default = "default_remote")]
    pub remote: String,
    /// One-time admission token delivered by the target node operator or provider.
    pub token: String,
    /// Optional label for the resulting grant on the remote node.
    #[serde(default)]
    pub label: Option<String>,
    /// Capabilities to request. Must be permitted by the target node's token file.
    #[serde(deserialize_with = "ryeos_runtime::scalar_or_vec::deserialize")]
    pub scopes: Vec<String>,
}

fn default_remote() -> String {
    "default".to_string()
}

pub async fn handle(req: Request, state: Arc<AppState>) -> Result<Value> {
    // Admission tokens are credentials delivered by a target operator. Only
    // operator-owned remote configuration may select where that credential is
    // released; a synchronized project must never shadow this coordinate.
    let remotes = config::load_remotes(&state.config.app_root)?;
    let remote_cfg = config::get_remote(&remotes, &req.remote)?;
    let client = RemoteClient::from_remote_cfg(&state, &remote_cfg);
    let live_identity = client.get_public_key().await?;
    live_identity.validate_configured_identity(&remote_cfg)?;

    let local_public_key = format!(
        "ed25519:{}",
        base64::engine::general_purpose::STANDARD.encode(state.identity.verifying_key().as_bytes())
    );
    let resp = client
        .admission_claim(
            &req.token,
            &local_public_key,
            req.label.as_deref(),
            &req.scopes,
        )
        .await?;

    Ok(serde_json::json!({
        "remote": req.remote,
        "url": remote_cfg.url,
        "local_public_key": local_public_key,
        "local_site_id": state.threads.site_id(),
        "claim": resp,
    }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:remote/admit",
    endpoint: "remote.admit",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.remote/admit"],
    handler: |params, _ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, state).await
        })
    },
};
