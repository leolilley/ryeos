//! `remote/push` — push project content to a remote node.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use crate::remote::client::RemoteClient;
use crate::remote::push::push_project;
use ryeos_executor::executor::ServiceAvailability;
use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;

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

    let project_path_str = req.project_path.as_deref().unwrap_or(".");
    let project_path = PathBuf::from(project_path_str);
    let abs_project_path = if project_path.is_absolute() {
        project_path
    } else {
        std::env::current_dir()?.join(&project_path)
    };

    let result = push_project(
        &client,
        &state,
        &abs_project_path,
        project_path_str,
        &state.ignore_matcher,
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
    handler: |params, state| {
        Box::pin(async move {
            let req: Request = serde_json::from_value(params)?;
            handle(req, state).await
        })
    },
};
