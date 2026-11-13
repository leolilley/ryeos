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
    /// Local project path the CLI may forward. Remote pins are node-level
    /// records, so a project never changes the listing.
    #[serde(default, alias = "project")]
    pub project_path: Option<PathBuf>,
    /// Pass-through for the CLI's `--no-project` flag. Has no effect.
    #[serde(default)]
    pub no_project: bool,
}

pub async fn handle(_req: Request, state: Arc<AppState>) -> Result<Value> {
    let report = config::load_remotes_report(&state.config.app_root)?;

    let mut entries: Vec<Value> = report
        .remotes
        .values()
        .map(|loaded| {
            let r = &loaded.config;
            serde_json::json!({
                "name": r.name,
                "url": r.url,
                "principal_id": r.principal_id,
                "site_id": r.site_id,
                "pin_source": loaded.pin_source.as_str(),
                "pinned_at": loaded.pinned_at,
                "config_path": loaded.config_path,
            })
        })
        .collect();
    entries.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    let mut invalid: Vec<Value> = report
        .invalid
        .into_iter()
        .map(|entry| {
            serde_json::json!({
                "name": entry.name,
                "code": entry.code,
                "config_path": entry.config_path,
                "url": entry.url,
                "error": entry.error,
                "repair_hint": entry.repair_hint,
            })
        })
        .collect();
    invalid.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));

    Ok(serde_json::json!({
        "remotes": entries,
        "invalid_remotes": invalid,
    }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:remote/list",
    endpoint: "remote.list",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.remote/list"],
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
