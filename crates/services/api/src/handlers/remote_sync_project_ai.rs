//! `remote/sync-project-ai` — push and apply managed project `.ai` content.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::registry::ServiceDescriptor;
use crate::remote::client::RemoteClient;
use crate::remote::config::{self, ProjectSyncScope};
use crate::remote::push::push_project_ai_only;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    #[serde(default = "default_remote")]
    pub remote: String,
    pub project: PathBuf,
    #[serde(default)]
    pub expected_deployed_hash: Option<String>,
    #[serde(default)]
    pub force: bool,
}

fn default_remote() -> String {
    "default".to_string()
}

pub async fn handle(req: Request, state: Arc<AppState>) -> Result<Value> {
    let remotes = config::load_remotes(&state.config.system_space_dir)?;
    let remote_cfg = config::get_remote(&remotes, &req.remote)?;
    let binding = config::resolve_project_binding(&remote_cfg, &req.project)?;
    if binding.sync_scope != ProjectSyncScope::AiOnly {
        anyhow::bail!(
            "remote sync-project-ai requires an ai_only binding for '{}'; got {:?}",
            binding.local_project_path.display(),
            binding.sync_scope
        );
    }

    let client = RemoteClient::from_named_remote(&state, &req.remote)?;
    let expected_deployed_hash = if req.force || req.expected_deployed_hash.is_some() {
        req.expected_deployed_hash.clone()
    } else {
        let status = client.project_status(&binding.remote_project_path).await?;
        if status
            .get("deployed")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            Some(
                status
                    .get("deployed_snapshot_hash")
                    .and_then(|v| v.as_str())
                    .context(
                        "project.status reported deployed=true without deployed_snapshot_hash",
                    )?
                    .to_string(),
            )
        } else {
            // Empty string is an apply-snapshot sentinel for "expect no deployed ref".
            Some(String::new())
        }
    };
    let push = push_project_ai_only(
        &client,
        &state,
        &binding.local_project_path,
        &binding.remote_project_path,
    )
    .await?;

    let apply = client
        .apply_project_snapshot(
            &binding.remote_project_path,
            &push.snapshot_hash,
            expected_deployed_hash.as_deref(),
            req.force,
        )
        .await?;

    Ok(serde_json::json!({
        "remote": req.remote,
        "local_project_path": binding.local_project_path,
        "remote_project_path": binding.remote_project_path,
        "push": {
            "snapshot_hash": push.snapshot_hash,
            "manifest_hash": push.manifest_hash,
            "manifest_entries": push.manifest_entries,
            "blobs_uploaded": push.blobs_uploaded,
            "blobs_skipped": push.blobs_skipped,
        },
        "expected_deployed_hash": expected_deployed_hash,
        "apply": apply,
    }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:remote/sync-project-ai",
    endpoint: "remote.sync-project-ai",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.remote.sync-project-ai"],
    handler: |params, _ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, state).await
        })
    },
};
