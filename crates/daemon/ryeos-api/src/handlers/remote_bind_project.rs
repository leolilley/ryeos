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
    let local_key = config::local_project_identity(&local)?.to_owned();
    let app_root = state.config.runtime_root();

    let mut remotes = config::load_remotes(app_root.as_path())?;
    if !remotes.contains_key(&req.remote) {
        let report = config::load_remotes_layered_report(app_root.as_path(), Some(&local))?;
        let loaded = config::get_loaded_remote(&report.remotes, &req.remote).map_err(|_| {
            anyhow::anyhow!(
                "remote '{}' not found in operator config; run `ryeos remote configure {}` first",
                req.remote,
                req.remote,
            )
        })?;
        let mut cfg = loaded.config;
        cfg.project_bindings.clear();
        remotes.insert(req.remote.clone(), cfg);
    }
    let remote = remotes
        .get_mut(&req.remote)
        .expect("remote is present after read-through or prior user config load");
    remote.project_bindings.insert(
        local_key.clone(),
        RemoteProjectBinding {
            remote_project_path: req.remote_project.clone(),
            sync_scope: req.sync_scope,
        },
    );
    config::save_remotes(app_root.as_path(), &remotes)?;

    Ok(serde_json::json!({
        "remote": req.remote,
        "scope": "operator",
        "local_project_path": local_key,
        "remote_project_path": req.remote_project,
        "sync_scope": req.sync_scope,
    }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:remote/bind-project",
    endpoint: "remote.bind-project",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.remote/bind-project"],
    handler: |params, _ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, state).await
        })
    },
};
