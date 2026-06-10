//! `remote/push` — push project content to a remote node.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use crate::registry::ServiceDescriptor;
use crate::remote::client::RemoteClient;
use crate::remote::config;
use crate::remote::push::{push_project, push_project_ai_only};
use ryeos_app::ignore::IgnoreMatcher;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;
use ryeos_state::project_sync::ProjectSyncScope;

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

    let client = RemoteClient::from_named_remote(&state, &req.remote, Some(&abs_project_path))?;

    // Resolve scope and remote-path-for-ref from any configured binding.
    // Default scope is `AiOnly` (see `ProjectSyncScope::default`) so an
    // unbound `remote push` ships only the `.ai/` subtree, never the
    // surrounding codebase or asset trees.
    let report = config::load_remotes_layered_report(&state.config.app_root, Some(&req.project))?;
    let loaded_remote = config::get_loaded_remote(&report.remotes, &req.remote).ok();
    let remote_cfg = loaded_remote.as_ref().map(|loaded| loaded.config.clone());
    let mut scope = ProjectSyncScope::default();
    if let Some(loaded) = loaded_remote.as_ref() {
        match config::resolve_loaded_project_binding(loaded, &abs_project_path) {
            Ok(binding) => {
                scope = binding.sync_scope;
                project_path_for_ref = binding.remote_project_path;
            }
            Err(_) => {
                // No binding is required for the default ai_only push; use
                // the canonical local project path as the remote path ref.
            }
        }
    }

    match scope {
        ProjectSyncScope::AiOnly => {
            // Apply remote ignore rules client-side so `.pyc`,
            // `__pycache__/`, etc. are dropped before we POST a
            // manifest the remote would reject.
            let remote_ignore = match remote_cfg.as_ref() {
                Some(cfg) => Some(IgnoreMatcher::from_config(&cfg.ingest_ignore)?),
                None => None,
            };
            let result = push_project_ai_only(
                &client,
                &state,
                &abs_project_path,
                &project_path_for_ref,
                remote_ignore.as_ref(),
            )
            .await?;
            Ok(serde_json::json!({
                "scope": "ai_only",
                "remote_project_path": project_path_for_ref,
                "snapshot_hash": result.snapshot_hash,
                "manifest_entries": result.manifest_entries,
                "blobs_uploaded": result.blobs_uploaded,
                "blobs_skipped": result.blobs_skipped,
            }))
        }
        ProjectSyncScope::FullProject => {
            // Full-project push requires the remote's ingest ignore rules.
            // Fetch inline on miss, but do not persist partial/invalid remote
            // config. Operators should use `remote configure` for durable config.
            let remote_ignore = match remote_cfg {
                Some(cfg) => IgnoreMatcher::from_config(&cfg.ingest_ignore)?,
                None => {
                    let fetched = client.get_ingest_ignore().await.map_err(|e| {
                        anyhow::anyhow!(
                            "no remote config for '{}' and inline fetch failed: {e:#} \
                             — run `ryeos remote configure --remote {}` first",
                            req.remote,
                            req.remote
                        )
                    })?;
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
                "scope": "full_project",
                "remote_project_path": project_path_for_ref,
                "snapshot_hash": result.snapshot_hash,
                "manifest_entries": result.manifest_entries,
                "blobs_uploaded": result.blobs_uploaded,
                "blobs_skipped": result.blobs_skipped,
            }))
        }
    }
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
