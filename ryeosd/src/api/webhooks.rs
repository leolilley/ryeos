use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::execution::project_source::{self, ProjectSource};
use crate::policy;
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct CreateWebhookRequest {
    pub item_id: String,
    pub project_path: String,
    pub description: Option<String>,
    pub secret_envelope: Option<Value>,
    #[serde(default)]
    pub project_source: Option<ProjectSource>,
}

pub async fn create_webhook(
    State(state): State<AppState>,
    request: axum::http::Request<axum::body::Body>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let principal_fp = policy::request_principal_id(&request, &state);
    let caller_scopes = policy::request_scopes(&request);
    policy::require_scope(&caller_scopes, "webhooks")?;

    let body_bytes = axum::body::to_bytes(request.into_body(), 10 * 1024 * 1024)
        .await
        .map_err(|_| (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(json!({ "error": "request body too large" })),
        ))?;
    let req: CreateWebhookRequest = serde_json::from_slice(&body_bytes)
        .map_err(|err| (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": err.to_string() })),
        ))?;

    if !req.item_id.contains(':') {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(
                json!({ "error": "item_id must be a canonical ref (e.g. directive:email/handle)" }),
            ),
        ));
    }

    let normalized_path = project_source::normalize_project_path(&req.project_path);
    let project_path_str = normalized_path.to_string_lossy().to_string();

    let remote_name = state.config.bind.to_string();
    let result = state
        .webhook_store()
        .create_binding(
            &principal_fp,
            &remote_name,
            &req.item_id,
            &project_path_str,
            req.description.as_deref(),
            req.secret_envelope.as_ref(),
            &principal_fp,
            req.project_source.unwrap_or_default(),
        )
        .map_err(policy::internal_error)?;

    Ok(Json(serde_json::to_value(result).unwrap()))
}

pub async fn list_webhooks(
    State(state): State<AppState>,
    request: axum::http::Request<axum::body::Body>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let principal_fp = policy::request_principal_id(&request, &state);
    let caller_scopes = policy::request_scopes(&request);
    policy::require_scope(&caller_scopes, "webhooks")?;
    let remote_name = state.config.bind.to_string();
    let bindings = state
        .webhook_store()
        .list_bindings(&principal_fp, &remote_name)
        .map_err(policy::internal_error)?;
    Ok(Json(json!({ "bindings": bindings })))
}

/// Inbound webhook dispatch — receives a webhook payload, verifies HMAC,
/// resolves the binding, and triggers execution via the unified runner.
pub async fn inbound_webhook(
    State(state): State<AppState>,
    Path(hook_id): Path<String>,
    request: axum::http::Request<axum::body::Body>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let signature_header = request
        .headers()
        .get("x-rye-signature")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    let body_bytes = axum::body::to_bytes(request.into_body(), 10 * 1024 * 1024)
        .await
        .map_err(|_| (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(json!({ "error": "request body too large" })),
        ))?;

    // Resolve binding — use generic error to avoid leaking hook existence
    let binding = state
        .webhook_store()
        .resolve_binding(&hook_id)
        .map_err(policy::internal_error)?
        .ok_or_else(|| (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "webhook authentication failed" })),
        ))?;

    // HMAC signature is REQUIRED for inbound webhooks
    if signature_header.is_empty() {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "webhook authentication failed" })),
        ));
    }
    let valid = state
        .webhook_store()
        .verify_hmac(&hook_id, &body_bytes, &signature_header)
        .map_err(policy::internal_error)?;
    if !valid {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "webhook authentication failed" })),
        ));
    }

    // Parse webhook payload and inject into execution parameters
    let payload: Value = serde_json::from_slice(&body_bytes).unwrap_or(json!({}));
    let exec_parameters = json!({ "webhook_payload": payload, "hook_id": &hook_id });

    // Resolve project execution context — for pushed_head, checks out from CAS
    let project_path = project_source::normalize_project_path(&binding.project_path);
    let checkout_id = format!("wh-{}-{}-{:08x}", hook_id, chrono::Utc::now().timestamp_millis(), rand::random::<u32>());
    let project_ctx = project_source::resolve_project_context(
        &state,
        &binding.project_source,
        &project_path,
        &binding.user_id,
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

    // Build execution through the unified runner pipeline
    let site_id = state.threads.site_id();

    let resolved = crate::services::thread_lifecycle::resolve_root_execution(
        &state.engine,
        site_id,
        &project_ctx.effective_path,
        &binding.item_id,
        "detached",
        exec_parameters.clone(),
        Some(binding.user_id.clone()),
        vec!["execute".to_string()],
        false,
    )
    .map_err(|err| {
        if let Some(ref d) = project_ctx.temp_dir {
            let _ = std::fs::remove_dir_all(d);
        }
        (StatusCode::BAD_REQUEST, Json(json!({ "error": err.to_string() })))
    })?;

    // Resolve vault secrets from item-declared required_secrets
    let required_secrets = &resolved.resolved_item.metadata.required_secrets;
    let vault_bindings = if !required_secrets.is_empty() {
        state
            .vault_store()
            .resolve_vault_env(&binding.user_id, required_secrets)
            .map_err(|err| {
                if let Some(ref d) = project_ctx.temp_dir {
                    let _ = std::fs::remove_dir_all(d);
                }
                policy::internal_error(err)
            })?
    } else {
        std::collections::HashMap::new()
    };

    let params = crate::execution::runner::ExecutionParams {
        resolved,
        acting_principal: binding.user_id.clone(),
        project_path: project_ctx.original_path,
        vault_bindings,
        snapshot_hash: project_ctx.snapshot_hash,
        item_ref: binding.item_id.clone(),
        parameters: exec_parameters,
        temp_dir: project_ctx.temp_dir,
    };

    let result = crate::execution::runner::run_detached(state.clone(), params)
        .await
        .map_err(|err| policy::internal_error(err.into()))?;

    Ok(Json(json!({
        "accepted": true,
        "hook_id": hook_id,
        "item_id": binding.item_id,
        "project_path": binding.project_path,
        "thread_id": result.running_thread.thread_id,
    })))
}

pub async fn revoke_webhook(
    State(state): State<AppState>,
    Path(hook_id): Path<String>,
    request: axum::http::Request<axum::body::Body>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let principal_fp = policy::request_principal_id(&request, &state);
    let caller_scopes = policy::request_scopes(&request);
    policy::require_scope(&caller_scopes, "webhooks")?;
    let remote_name = state.config.bind.to_string();
    let revoked = state
        .webhook_store()
        .revoke_binding(&hook_id, &principal_fp, &remote_name)
        .map_err(policy::internal_error)?;
    if !revoked {
        return Err((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "Webhook not found or already revoked" })),
        ));
    }
    Ok(Json(json!({ "revoked": hook_id })))
}
