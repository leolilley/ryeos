//! `remote/push` — push project content to a remote node.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use crate::remote::client::RemoteClient;
use crate::remote::config;
use crate::remote::push::push_project;
use ryeos_executor::executor::ServiceAvailability;
use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_app::ignore::IgnoreMatcher;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// Remote name (default: "default").
    #[serde(default = "default_remote")]
    pub remote: String,
    /// `--project <abs>` from the CLI. Required: pushing nothing
    /// is a no-op so `--no-project` is not accepted here. CLI must
    /// canonicalize before sending.
    pub project: PathBuf,
}

fn default_remote() -> String {
    "default".to_string()
}

pub async fn handle(req: Request, state: Arc<AppState>) -> Result<Value> {
    let client = RemoteClient::from_named_remote(&state, &req.remote)?;

    let path = &req.project;
    if !path.is_absolute() {
        return Err(anyhow::anyhow!(
            "project '{}' is not absolute: the CLI must canonicalize \
             before sending. The daemon's cwd is irrelevant to the caller.",
            path.display()
        ));
    }
    let canonical_abs = path.canonicalize().map_err(|e| {
        anyhow::anyhow!(
            "cannot canonicalize project '{}': {}. Ensure the path \
             exists and is accessible.",
            path.display(),
            e
        )
    })?;
    let mut project_path_for_ref = canonical_abs.to_string_lossy().to_string();
    let abs_project_path = canonical_abs;

    // Load remote's cached ignore rules (required — remote configure always
    // populates them). Inline fetch on cache miss, persisting for future use.
    let remotes = config::load_remotes(&state.config.system_space_dir)?;
    let remote_cfg = config::get_remote(&remotes, &req.remote).ok();
    if let Some(cfg) = remote_cfg.as_ref() {
        if let Some(binding) = cfg.project_bindings.get(&project_path_for_ref) {
            match binding.sync_scope {
                ryeos_state::project_sync::ProjectSyncScope::AiOnly => {
                    anyhow::bail!(
                        "remote push is a full-project staging command and refuses ai_only binding '{}' -> '{}'; use `ryeos remote sync-project-ai`",
                        project_path_for_ref,
                        binding.remote_project_path
                    );
                }
                ryeos_state::project_sync::ProjectSyncScope::FullProject => {
                    config::validate_remote_project_path(&binding.remote_project_path)?;
                    project_path_for_ref = binding.remote_project_path.clone();
                }
            }
        }
    }
    let remote_ignore = match remote_cfg {
        Some(cfg) => IgnoreMatcher::from_config(&cfg.ingest_ignore)?,
        None => {
            // No config at all — fetch inline and persist.
            let fetched = client.get_ingest_ignore().await
                .map_err(|e| anyhow::anyhow!(
                    "no remote config for '{}' and inline fetch failed: {e:#} \
                     — run `ryeos remote configure --remote {}` first",
                    req.remote, req.remote
                ))?;
            let _ = (|| -> Result<()> {
                let mut remotes = config::load_remotes(&state.config.system_space_dir)?;
                remotes.insert(req.remote.clone(), config::RemoteConfig {
                    name: req.remote.clone(),
                    url: client.base_url().to_string(),
                    principal_id: String::new(), // partial — user should reconfigure
                    vault_fingerprint: String::new(),
                    ingest_ignore: fetched.clone(),
                    project_bindings: HashMap::new(),
                });
                config::save_remotes(&state.config.system_space_dir, &remotes)
            })();
            IgnoreMatcher::from_config(&fetched)?
        }
    };

    let result = push_project(
        &client,
        &state,
        &abs_project_path,
        &project_path_for_ref,
        &remote_ignore,
    )
    .await?;

    Ok(serde_json::json!({
        "snapshot_hash": result.snapshot_hash,
        "manifest_entries": result.manifest_entries,
        "blobs_uploaded": result.blobs_uploaded,
        "blobs_skipped": result.blobs_skipped,
    }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:remote/push",
    endpoint: "remote.push",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.remote.push"],
    handler: |params, _ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, state).await
        })
    },
};
