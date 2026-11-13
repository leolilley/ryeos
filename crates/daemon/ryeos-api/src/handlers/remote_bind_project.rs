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

    // A project's remotes file is checked even though binding only writes
    // node config: one naming remote trust material is refused here too.
    config::load_remotes_layered_report(app_root.as_path(), Some(&local))?;
    let mut remotes = config::load_remotes(app_root.as_path())?;
    // Only the node's own remotes file defines a remote; a project's remotes
    // file is never copied into it.
    let Some(remote) = remotes.get_mut(&req.remote) else {
        anyhow::bail!(
            "remote '{}' not found in the node's remotes config; run `ryeos remote configure {} --url <https-url>` first",
            req.remote,
            req.remote,
        );
    };
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
