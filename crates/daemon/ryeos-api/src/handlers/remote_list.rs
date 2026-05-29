//! `remote/list` — list configured remote nodes.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use crate::registry::ServiceDescriptor;
use crate::remote::config;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

#[derive(serde::Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Request {
    /// Optional local project path. When supplied, project-level
    /// remotes from `<project>/.ai/config/remotes/remotes.yaml` are
    /// merged on top of user-level remotes (project wins on name
    /// collision). The CLI injects this automatically via the
    /// alias's `project_resolution: optional` field.
    #[serde(default, alias = "project")]
    pub project_path: Option<PathBuf>,
    /// Pass-through for the CLI's `--no-project` flag. Ignored here;
    /// project layering is opt-in via `project_path`.
    #[serde(default)]
    pub no_project: bool,
}

pub async fn handle(req: Request, state: Arc<AppState>) -> Result<Value> {
    let project = if req.no_project {
        None
    } else {
        req.project_path.as_deref()
    };
    let remotes = config::load_remotes_layered(&state.config.system_space_dir, project)?;

    let mut entries: Vec<Value> = remotes
        .values()
        .map(|r| {
            serde_json::json!({
                "name": r.name,
                "url": r.url,
                "principal_id": r.principal_id,
            })
        })
        .collect();
    entries.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));

    Ok(serde_json::json!({ "remotes": entries }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:remote/list",
    endpoint: "remote.list",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.remote.list"],
    handler: |params, _ctx, state| {
        Box::pin(async move {
            let req: Request = if params.is_null() {
                Request::default()
            } else {
                serde_json::from_value(params)?
            };
            handle(req, state).await
        })
    },
};
