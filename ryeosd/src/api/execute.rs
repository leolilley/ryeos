use std::collections::HashMap;

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::task;

use crate::execution::launch;
use crate::execution::project_source::{self, ProjectSource};
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
    /// How the project root is determined. Defaults to live FS.
    #[serde(default)]
    pub project_source: Option<ProjectSource>,
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
    let project_source = request.project_source.clone().unwrap_or_default();
    let project_path = match &request.project_path {
        Some(p) => project_source::normalize_project_path(p),
        None => {
            if matches!(project_source, ProjectSource::PushedHead) {
                return Err((
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": "project_path is required when project_source is pushed_head" })),
                ));
            }
            std::env::current_dir().map_err(|err| policy::internal_error(err.into()))?
        }
    };

    // Reject validate_only + pushed_head — we don't checkout from CAS for dry-run
    if request.validate_only && matches!(project_source, ProjectSource::PushedHead) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "validate_only is not supported with pushed_head project_source" })),
        ));
    }

    // Resolve project execution context BEFORE item resolution.
    // For pushed_head, this checks out from CAS so resolution runs against
    // the correct project root.
    let checkout_id = format!("pre-{}-{:08x}", chrono::Utc::now().timestamp_millis(), rand::random::<u32>());
    let project_ctx = project_source::resolve_project_context(
        &state,
        &project_source,
        &project_path,
        &caller_principal_id,
        &checkout_id,
    )
    .map_err(|err| {
        let msg = err.to_string();
        if msg.contains("push first") {
            (StatusCode::CONFLICT, Json(json!({ "error": msg })))
        } else {
            policy::internal_error(err)
        }
    })?;

    let mut resolved = thread_lifecycle::resolve_root_execution(
            &state.engine,
            site_id,
            &project_ctx.effective_path,
            &request.item_ref,
            &request.launch_mode,
            request.parameters.clone(),
            Some(caller_principal_id.clone()),
            caller_scopes,
            request.validate_only,
        )
        .map_err(|err| {
            if let Some(ref d) = project_ctx.temp_dir {
                let _ = std::fs::remove_dir_all(d);
            }
            invalid_request(err)
        })?;

    if let Some(target) = request.target_site_id {
        resolved.target_site_id = Some(target);
    }

    if !state.threads.kind_profiles().is_root_executable(&resolved.kind) {
        if let Some(ref d) = project_ctx.temp_dir {
            let _ = std::fs::remove_dir_all(d);
        }
        return Err(not_implemented(&format!(
            "kind '{}' is not root executable",
            resolved.kind
        )));
    }

    // Check if this resolves to a native runtime binary
    let runtime_binary = launch::native_runtime_binary(&resolved.executor_ref);
    if runtime_binary.is_some() {
        // Reject unsupported modes on native path
        if matches!(project_source, ProjectSource::PushedHead) {
            if let Some(ref d) = project_ctx.temp_dir {
                let _ = std::fs::remove_dir_all(d);
            }
            return Err((
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "pushed_head not yet supported for native runtimes" })),
            ));
        }
        if resolved.target_site_id.is_some() {
            if let Some(ref d) = project_ctx.temp_dir {
                let _ = std::fs::remove_dir_all(d);
            }
            return Err((
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "remote execution not yet supported for native runtimes" })),
            ));
        }
        if request.launch_mode == "detached" {
            if let Some(ref d) = project_ctx.temp_dir {
                let _ = std::fs::remove_dir_all(d);
            }
            return Err((
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "detached mode not yet supported for native runtimes" })),
            ));
        }
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

        if let Some(ref d) = project_ctx.temp_dir {
            let _ = std::fs::remove_dir_all(d);
        }

        return Ok(Json(json!({
            "validated": true,
            "item_ref": resolved.item_ref,
            "kind": resolved.kind,
            "executor_ref": resolved.executor_ref,
            "trust_class": validated.trust_class,
            "plan_id": validated.plan_id,
        })));
    }

    // Resolve vault secrets from item-declared required_secrets only.
    let required_secrets = &resolved.resolved_item.metadata.required_secrets;
    let vault_bindings = if !required_secrets.is_empty() {
        state
            .vault_store()
            .resolve_vault_env(&caller_principal_id, required_secrets)
            .map_err(|err| {
                if let Some(ref d) = project_ctx.temp_dir {
                    let _ = std::fs::remove_dir_all(d);
                }
                policy::internal_error(err)
            })?
    } else {
        HashMap::new()
    };

    let params = ExecutionParams {
        resolved,
        acting_principal: caller_principal_id,
        project_path: project_ctx.original_path,
        vault_bindings,
        snapshot_hash: project_ctx.snapshot_hash,
        item_ref: request.item_ref,
        parameters: request.parameters,
        temp_dir: project_ctx.temp_dir,
    };

    // Native runtime path
    if let Some(binary) = runtime_binary {
        let result = launch::build_and_launch(
            &state,
            &binary,
            &params.acting_principal,
            &params.resolved,
            &params.project_path,
            &params.parameters,
            &params.vault_bindings,
        )
        .map_err(|err| policy::internal_error(err))?;

        return Ok(Json(json!({
            "thread": result.thread,
            "result": result.result,
        })));
    }

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
