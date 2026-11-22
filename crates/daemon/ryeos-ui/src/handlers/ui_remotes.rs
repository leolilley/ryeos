//! `ui.ryeos.remotes.list` and `ui.ryeos.remotes.probe` — remotes
//! inspection for the ryeos-ui.
//!
//! `remotes.list` returns configured remotes
//! without probing. `remotes.probe` performs an on-demand status check
//! against a named remote.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use serde_json::Value;

use ryeos_api::registry::ServiceDescriptor;
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::handler_error::HandlerError;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

const REMOTE_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

fn retained_attachment()
-> Result<Arc<crate::browser_session::AdmittedBindingAttachment>, HandlerError> {
    crate::seat_auth::compiled_ui_attachment()
        .ok_or_else(|| HandlerError::Forbidden("compiled UI attachment required".into()))
}

// ── remotes.list ──────────────────────────────────────────────────

pub async fn handle_remotes_list(
    _params: Value,
    _ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    let _attachment = retained_attachment()?;
    remotes_list(&state.config.app_root)
}

/// The `remotes.list` projection. Remote pins are node-signed records; a
/// project never contributes remotes.
pub fn remotes_list(app_root: &std::path::Path) -> Result<Value> {
    let report = ryeos_api::remote::config::load_remotes_report(app_root)?;

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
                "project_bindings": r.project_bindings,
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

    Ok(serde_json::json!({ "remotes": entries, "invalid_remotes": invalid }))
}

// ── remotes.probe ──────────────────────────────────────────────────

pub async fn handle_remotes_probe(
    params: Value,
    _ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    let _attachment = retained_attachment()?;

    let req: ProbeRequest = serde_json::from_value(params)
        .map_err(|e| HandlerError::BadRequest(format!("invalid request: {e}")))?;

    let remotes = ryeos_api::remote::config::load_remotes(&state.config.app_root)?;
    let remote_cfg = ryeos_api::remote::config::get_remote(&remotes, &req.remote)?;

    let client = ryeos_api::remote::client::RemoteClient::from_remote_cfg(&state, &remote_cfg)?;

    let health = match tokio::time::timeout(REMOTE_PROBE_TIMEOUT, client.get_health()).await {
        Ok(Ok(health)) => health,
        Ok(Err(e)) => serde_json::json!({ "status": "error", "detail": format!("{e:#}") }),
        Err(_) => serde_json::json!({
            "status": "error",
            "detail": format!("remote probe timed out after {}s", REMOTE_PROBE_TIMEOUT.as_secs()),
        }),
    };

    Ok(serde_json::json!({
        "remote": {
            "name": req.remote,
            "url": remote_cfg.url,
            "principal_id": remote_cfg.principal_id,
        },
        "health": health,
    }))
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ProbeRequest {
    pub remote: String,
}

// ── Descriptors ────────────────────────────────────────────────────

pub const REMOTES_LIST_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:ui/ryeos-ui/remotes/list",
    endpoint: "ui.ryeos.remotes.list",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| {
        Box::pin(async move { handle_remotes_list(params, ctx, state).await })
    },
};

pub const REMOTES_PROBE_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:ui/ryeos-ui/remotes/probe",
    endpoint: "ui.ryeos.remotes.probe",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| {
        Box::pin(async move { handle_remotes_probe(params, ctx, state).await })
    },
};
