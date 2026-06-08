//! `remote/run` — execute an item against a configured remote project.

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::Value;

use crate::handler_error::{HandlerError, HandlerResult};
use crate::registry::ServiceDescriptor;
use crate::remote::client::RemoteClient;
use crate::remote::config;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// Remote name (default: "default").
    #[serde(default = "default_remote")]
    pub remote: String,
    /// Item to execute (canonical ref).
    pub item_ref: String,
    /// Local project path used to resolve the configured remote binding.
    pub project: PathBuf,
    /// Parameters for the item.
    #[serde(default)]
    pub parameters: Value,
}

fn default_remote() -> String {
    "default".to_string()
}

pub async fn handle(req: Request, state: Arc<AppState>) -> HandlerResult<Value> {
    let report =
        config::load_remotes_layered_report(&state.config.system_space_dir, Some(&req.project))
            .map_err(|e| HandlerError::Internal(format!("load remotes: {e:#}")))?;
    let loaded_remote = config::get_loaded_remote(&report.remotes, &req.remote)
        .map_err(|e| HandlerError::BadRequest(format!("remote '{}': {e:#}", req.remote)))?;
    let binding = config::resolve_loaded_project_binding(&loaded_remote, &req.project)
        .map_err(|e| HandlerError::BadRequest(format!("project binding: {e:#}")))?;
    let remote_cfg = loaded_remote.config;

    let client = RemoteClient::from_remote_cfg(&state, &remote_cfg);
    let remote_result = client
        .execute(
            &req.item_ref,
            &binding.remote_project_path,
            &req.parameters,
            "live_fs",
        )
        .await
        .map_err(|e| HandlerError::Internal(format!("remote run failed: {e:#}")))?;

    Ok(serde_json::json!({
        "remote": req.remote,
        "local_project_path": binding.local_project_path,
        "remote_project_path": binding.remote_project_path,
        "sync_scope": binding.sync_scope,
        "result": remote_result,
    }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:remote/run",
    endpoint: "remote.run",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.remote.admin"],
    handler: |params, _ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, state).await.map_err(Into::into)
        })
    },
};
