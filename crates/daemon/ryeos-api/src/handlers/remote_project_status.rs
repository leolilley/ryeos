//! `remote/project-status` — status wrapper for a bound remote project.

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
    #[serde(default = "default_remote")]
    pub remote: String,
    pub project: PathBuf,
}

fn default_remote() -> String {
    "default".to_string()
}

pub async fn handle(req: Request, state: Arc<AppState>) -> Result<Value> {
    let report = config::load_remotes_layered_report(&state.config.app_root, Some(&req.project))?;
    let loaded_remote = config::get_loaded_remote(&report.remotes, &req.remote)?;
    let remote_cfg = loaded_remote.config.clone();
    let binding = config::resolve_loaded_project_binding(&loaded_remote, &req.project)?;
    let client = RemoteClient::from_remote_cfg(&state, &remote_cfg);
    let status = client.project_status(&binding.remote_project_path).await?;

    Ok(serde_json::json!({
        "remote": req.remote,
        "local_project_path": binding.local_project_path,
        "remote_project_path": binding.remote_project_path,
        "status": status,
    }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:remote/project-status",
    endpoint: "remote.project-status",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.remote.project-status"],
    handler: |params, _ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, state).await
        })
    },
};
