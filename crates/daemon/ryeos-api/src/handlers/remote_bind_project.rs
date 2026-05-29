//! `remote/bind-project` — configure local-to-remote project mapping.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use crate::registry::ServiceDescriptor;
use crate::remote::config::{self, ProjectSyncScope, RemoteProjectBinding};
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    #[serde(default = "default_remote")]
    pub remote: String,
    pub project: PathBuf,
    pub remote_project: String,
    #[serde(default)]
    pub sync_scope: ProjectSyncScope,
}

fn default_remote() -> String {
    "default".to_string()
}

pub async fn handle(req: Request, state: Arc<AppState>) -> Result<Value> {
    config::validate_remote_project_path(&req.remote_project)?;
    let local = config::canonical_local_project_path(&req.project)?;
    let local_key = local.to_string_lossy().to_string();

    // Mutate the remote where it actually lives (project or user).
    let scope = config::locate_remote_scope(
        &state.config.system_space_dir,
        Some(&req.project),
        &req.remote,
    )?;
    let mut remotes = config::load_remotes_at(&config::remotes_config_path(&scope))?;
    let remote = remotes
        .get_mut(&req.remote)
        .ok_or_else(|| anyhow::anyhow!("remote '{}' not found in config", req.remote))?;
    remote.project_bindings.insert(
        local_key.clone(),
        RemoteProjectBinding {
            remote_project_path: req.remote_project.clone(),
            sync_scope: req.sync_scope,
        },
    );
    config::save_remotes(&scope, &remotes)?;

    Ok(serde_json::json!({
        "remote": req.remote,
        "local_project_path": local_key,
        "remote_project_path": req.remote_project,
        "sync_scope": req.sync_scope,
    }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:remote/bind-project",
    endpoint: "remote.bind-project",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.remote.bind-project"],
    handler: |params, _ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, state).await
        })
    },
};
