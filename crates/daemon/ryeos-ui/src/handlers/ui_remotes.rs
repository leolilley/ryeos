//! `ui.ryeos.remotes.list` and `ui.ryeos.remotes.probe` — remotes
//! inspection for the ryeos-ui.
//!
//! `remotes.list` returns configured remotes (with project-layer merging)
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

use crate::state::get_ui_state;

const REMOTE_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

fn session_id_from_context(ctx: &HandlerContext) -> Option<String> {
    ctx.fingerprint.strip_prefix("session:").map(String::from)
}

fn retained_session(
    ctx: &HandlerContext,
    state: &AppState,
) -> Result<crate::browser_session::BrowserSession, HandlerError> {
    if let Some(session) = crate::seat_auth::compiled_ui_session() {
        return Ok(session);
    }
    let session_id = session_id_from_context(ctx)
        .ok_or_else(|| HandlerError::Forbidden("compiled UI session required".into()))?;
    get_ui_state(state)
        .expect("UiState not set")
        .browser_sessions
        .get_session(&session_id)
        .ok_or(HandlerError::Forbidden("session expired or invalid".into()))
}

fn retained_project_access(
    session: &crate::browser_session::BrowserSession,
) -> Result<Option<crate::seat_auth::RetainedProjectAccess>> {
    Ok(crate::seat_auth::session_project_access(session)?)
}

// ── remotes.list ──────────────────────────────────────────────────

pub async fn handle_remotes_list(
    _params: Value,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    let session = retained_session(&ctx, &state)?;
    let project_access = retained_project_access(&session)?;
    let project = project_access.as_ref().map(|access| access.path());

    let report =
        ryeos_api::remote::config::load_remotes_layered_report(&state.config.app_root, project)?;

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
                "scope": loaded.scope.label(),
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
                "scope": entry.scope.label(),
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
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    let session = retained_session(&ctx, &state)?;
    let project_access = retained_project_access(&session)?;
    let project = project_access.as_ref().map(|access| access.path());

    let req: ProbeRequest = serde_json::from_value(params)
        .map_err(|e| HandlerError::BadRequest(format!("invalid request: {e}")))?;

    let remotes = ryeos_api::remote::config::load_remotes_layered(&state.config.app_root, project)?;
    let remote_cfg = ryeos_api::remote::config::get_remote(&remotes, &req.remote)?;

    let client = ryeos_api::remote::client::RemoteClient::from_remote_cfg(&state, &remote_cfg);

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
