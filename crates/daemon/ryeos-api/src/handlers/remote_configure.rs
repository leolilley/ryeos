//! `remote/configure` — add or update a remote node config.

use std::path::PathBuf;
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
    /// When supplied, the entry is written to the project-level
    /// remotes config at `<project>/.ai/config/remotes/remotes.yaml`
    /// instead of the user-level system space. The CLI injects this
    /// automatically via the alias's `project_resolution: optional`
    /// field; pass `--no-project` to force user-level.
    #[serde(default, alias = "project")]
    pub project_path: Option<PathBuf>,
    /// Pass-through for the CLI's `--no-project` flag.
    #[serde(default)]
    pub no_project: bool,
}

pub async fn handle(req: Request, state: Arc<AppState>) -> Result<Value> {
    config::validate_url(&req.url)?;

    // Discover the remote's public key
    let client = RemoteClient::new(&req.url, "", state.identity.clone());
    let pubkey = client.get_public_key().await?;

    // Fetch the remote's ingest-ignore rules — required for push to work
    // correctly. Fail hard if unavailable.
    let ingest_ignore = client.get_ingest_ignore().await?;

    // Resolve target root: project-level if a project_path is bound and
    // --no-project was not requested, else user-level system space.
    let target_root: PathBuf = match (req.no_project, req.project_path.as_ref()) {
        (false, Some(p)) => p.clone(),
        _ => state.config.system_space_dir.clone(),
    };
    let target_label = if target_root == state.config.system_space_dir {
        "user"
    } else {
        "project"
    };

    let mut remotes = config::load_remotes_at(&config::remotes_config_path(&target_root))?;
    let vault_fp = pubkey.vault_fingerprint.clone();
    remotes.insert(
        req.remote.clone(),
        config::RemoteConfig {
            name: req.remote.clone(),
            url: req.url.clone(),
            principal_id: pubkey.principal_id.clone(),
            site_id: pubkey.site_id.clone(),
            vault_fingerprint: pubkey.vault_fingerprint,
            ingest_ignore,
            project_bindings: remotes
                .get(&req.remote)
                .map(|r| r.project_bindings.clone())
                .unwrap_or_default(),
        },
    );
    config::save_remotes(&target_root, &remotes)?;

    Ok(serde_json::json!({
        "configured": req.remote,
        "scope": target_label,
        "config_path": config::remotes_config_path(&target_root),
        "url": req.url,
        "principal_id": pubkey.principal_id,
        "fingerprint": pubkey.fingerprint,
        "vault_fingerprint": vault_fp,
        "site_id": pubkey.site_id,
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
