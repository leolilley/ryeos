//! `remote/push` — push project content to a remote node.

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
    /// Project path to push. Defaults to the caller's project_path.
    pub project_path: Option<String>,
}

fn default_remote() -> String {
    "default".to_string()
}

pub async fn handle(req: Request, state: Arc<AppState>) -> Result<Value> {
    let client = RemoteClient::from_named_remote(&state, &req.remote)?;

    // §0a: canonicalize project_path the same way remote_execute does,
    // so push HEAD ref keys agree with what execute later reads.
    let raw_path = req.project_path
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!(
            "remote_push requires --project-path. Pass `-p <path>` explicitly; \
             defaulting to cwd was removed because the daemon's cwd is \
             irrelevant to the caller."
        ))?;
    let project_path = PathBuf::from(raw_path);
    let abs_project_path = if project_path.is_absolute() {
        project_path
    } else {
        std::env::current_dir()?.join(&project_path)
    };
    let canonical_abs = abs_project_path.canonicalize()
        .map_err(|e| anyhow::anyhow!(
            "cannot canonicalize project_path '{}': {}. Ensure the path \
             exists and is accessible.",
            abs_project_path.display(), e
        ))?;
    let project_path_for_ref = canonical_abs.to_string_lossy().to_string();
    let abs_project_path = canonical_abs;

    // Load remote's cached ignore rules (required — remote configure always
    // populates them). Inline fetch on cache miss, persisting for future use.
    let remotes = config::load_remotes(&state.config.system_space_dir)?;
    let remote_cfg = config::get_remote(&remotes, &req.remote).ok();
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
