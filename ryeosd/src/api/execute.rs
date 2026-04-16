use std::collections::HashMap;

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::task;

use crate::execution::runner::{self, ExecutionParams};
use crate::policy;
use crate::services::thread_lifecycle;
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct ExecuteRequest {
    pub item_ref: String,
    /// Project root path for three-tier resolution. Required when the caller
    /// (e.g. MCP server) runs in a different cwd than the user's project.
    #[serde(default)]
    pub project_path: Option<String>,
    #[serde(default)]
    pub parameters: Value,
    #[serde(default = "default_launch_mode")]
    pub launch_mode: String,
    #[serde(default)]
    pub target_site_id: Option<String>,
    #[serde(default)]
    pub validate_only: bool,
    /// Vault secret keys to resolve and inject as env vars.
    #[serde(default)]
    pub vault_keys: Option<Vec<String>>,
    /// CAS project snapshot hash — if set, checkout from CAS instead of local path.
    #[serde(default)]
    pub project_snapshot_hash: Option<String>,
}

fn default_launch_mode() -> String {
    "inline".to_string()
}

pub async fn execute(
    State(state): State<AppState>,
    request: axum::http::Request<axum::body::Body>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let caller_principal_id = policy::request_principal_id(&request, &state);
    let caller_scopes = policy::request_scopes(&request);
    policy::require_scope(&caller_scopes, "execute")?;

    // Deserialize the body
    let body_bytes = axum::body::to_bytes(request.into_body(), 10 * 1024 * 1024)
        .await
        .map_err(|_| (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(json!({ "error": "request body too large" })),
        ))?;
    let request: ExecuteRequest = serde_json::from_slice(&body_bytes)
        .map_err(|err| invalid_request(err.into()))?;

    let site_id = state.threads.site_id();
    let project_path = match &request.project_path {
        Some(p) => std::path::PathBuf::from(p),
        None => std::env::current_dir().map_err(|err| policy::internal_error(err.into()))?,
    };
    let mut resolved = thread_lifecycle::resolve_root_execution(
            &state.engine,
            site_id,
            &project_path,
            &request.item_ref,
            &request.launch_mode,
            request.parameters.clone(),
            Some(caller_principal_id.clone()),
            caller_scopes,
            request.validate_only,
        )
        .map_err(invalid_request)?;

    if let Some(target) = request.target_site_id {
        resolved.target_site_id = Some(target);
    }

    if !state.threads.kind_profiles().is_root_executable(&resolved.kind) {
        return Err(not_implemented(&format!(
            "kind '{}' is not root executable",
            resolved.kind
        )));
    }

    if request.validate_only {
        let engine = state.engine.clone();
        let resolved_clone = resolved.clone();
        let validated = task::spawn_blocking(move || {
            thread_lifecycle::validate_item(&engine, &resolved_clone)
        })
        .await
        .map_err(|err| policy::internal_error(err.into()))?
        .map_err(invalid_request)?;

        return Ok(Json(json!({
            "validated": true,
            "item_ref": resolved.item_ref,
            "kind": resolved.kind,
            "executor_ref": resolved.executor_ref,
            "trust_class": validated.trust_class,
            "plan_id": validated.plan_id,
        })));
    }

    // Resolve vault env vars
    let vault_bindings = match &request.vault_keys {
        Some(keys) if !keys.is_empty() => state
            .vault_store()
            .resolve_vault_env(&caller_principal_id, keys)
            .map_err(policy::internal_error)?,
        _ => HashMap::new(),
    };

    let params = ExecutionParams {
        resolved,
        acting_principal: caller_principal_id,
        project_path,
        vault_bindings,
        project_snapshot_hash: request.project_snapshot_hash,
        item_ref: request.item_ref,
        parameters: request.parameters,
    };

    if request.launch_mode == "detached" {
        let result = runner::run_detached(state.clone(), params)
            .await
            .map_err(|err| policy::internal_error(err.into()))?;

        return Ok(Json(json!({
            "thread": result.running_thread,
            "detached": true,
        })));
    }

    // Inline execution
    let result = runner::run_inline(state.clone(), params)
        .await
        .map_err(|err| policy::internal_error(err.into()))?;

    Ok(Json(json!({
        "thread": result.finalized_thread,
        "result": result.result,
    })))
}

fn invalid_request(err: anyhow::Error) -> (StatusCode, Json<Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": err.to_string() })),
    )
}

fn not_implemented(message: &str) -> (StatusCode, Json<Value>) {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(json!({ "error": message })),
    )
}
