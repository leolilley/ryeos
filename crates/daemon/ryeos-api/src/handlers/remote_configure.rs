//! `remote/configure` — add or update a remote node config.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
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
    #[serde(default)]
    pub remote: Option<String>,
    /// URL of the remote node (HTTPS required except for loopback).
    #[serde(default)]
    pub url: Option<String>,
    /// Path to a remote descriptor YAML. The descriptor is a trust pin,
    /// not a credential; configure still verifies the live node matches it.
    #[serde(default)]
    pub descriptor: Option<PathBuf>,
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
    let descriptor = req
        .descriptor
        .as_deref()
        .map(config::RemoteDescriptor::from_path)
        .transpose()?;

    let remote_name = req
        .remote
        .clone()
        .or_else(|| descriptor.as_ref().and_then(|d| d.name.clone()))
        .context("remote name required: pass a remote name or descriptor.name")?;
    let remote_url = req
        .url
        .clone()
        .or_else(|| descriptor.as_ref().map(|d| d.url.clone()))
        .context("remote URL required: pass --url or descriptor.url")?;
    config::validate_url(&remote_url)?;

    // Discover the remote's public key
    let client = RemoteClient::new(&remote_url, "", state.identity.clone());
    let pubkey = client.get_public_key().await?;
    pubkey.validate_identity_binding()?;

    if let Some(descriptor) = descriptor.as_ref() {
        if pubkey.signing_key != descriptor.node.public_key {
            anyhow::bail!(
                "remote descriptor key mismatch for '{}': descriptor pins {}, live node reports {}",
                remote_name,
                descriptor.node.public_key,
                pubkey.signing_key,
            );
        }
        if pubkey.fingerprint != descriptor.node.fingerprint {
            anyhow::bail!(
                "remote descriptor fingerprint mismatch for '{}': descriptor pins {}, live node reports {}",
                remote_name,
                descriptor.node.fingerprint,
                pubkey.fingerprint,
            );
        }
    }

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

    let config_path = config::remotes_config_path(&target_root);
    let mut remotes = config::load_remotes_at(&config_path)?;
    let existing_project_bindings = if let Some(remote) = remotes.get(&remote_name) {
        remote.project_bindings.clone()
    } else {
        config::load_remote_project_bindings_at(&config_path, &remote_name)?
    };
    let vault_fp = pubkey.vault_fingerprint.clone();
    let signing_key = pubkey.signing_key.clone();
    let remote_config = config::RemoteConfig {
        name: remote_name.clone(),
        url: remote_url.clone(),
        principal_id: pubkey.principal_id.clone(),
        signing_key: pubkey.signing_key,
        site_id: pubkey.site_id.clone(),
        vault_fingerprint: pubkey.vault_fingerprint,
        ingest_ignore,
        project_bindings: existing_project_bindings,
    };
    remote_config.validate()?;
    remotes.insert(remote_name.clone(), remote_config);
    config::save_remotes(&target_root, &remotes)?;

    Ok(serde_json::json!({
        "configured": remote_name,
        "scope": target_label,
        "config_path": config::remotes_config_path(&target_root),
        "url": remote_url,
        "principal_id": pubkey.principal_id,
        "signing_key": signing_key,
        "fingerprint": pubkey.fingerprint,
        "vault_fingerprint": vault_fp,
        "site_id": pubkey.site_id,
        "descriptor_verified": descriptor.is_some(),
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
