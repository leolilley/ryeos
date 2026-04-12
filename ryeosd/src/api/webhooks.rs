use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::auth::Principal;
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct CreateWebhookRequest {
    pub item_id: String,
    pub project_path: String,
    pub description: Option<String>,
    pub secret_envelope: Option<Value>,
    pub vault_keys: Option<Vec<String>>,
}

pub async fn create_webhook(
    State(state): State<AppState>,
    request: axum::http::Request<axum::body::Body>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let principal_fp = extract_principal_fp(&request, &state);

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

    let remote_name = state.config.bind.to_string();
    let result = state
        .webhook_store()
        .create_binding(
            &principal_fp,
            &remote_name,
            &req.item_id,
            &req.project_path,
            req.description.as_deref(),
            req.secret_envelope.as_ref(),
            "",
            req.vault_keys.as_deref(),
        )
        .map_err(|err| {
            tracing::error!(error = %err, "internal error in webhook handler");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "internal server error" })),
            )
        })?;

    Ok(Json(serde_json::to_value(result).unwrap()))
}

pub async fn list_webhooks(
    State(state): State<AppState>,
    request: axum::http::Request<axum::body::Body>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let principal_fp = extract_principal_fp(&request, &state);
    let remote_name = state.config.bind.to_string();
    let bindings = state
        .webhook_store()
        .list_bindings(&principal_fp, &remote_name)
        .map_err(|err| {
            tracing::error!(error = %err, "internal error in webhook handler");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "internal server error" })),
            )
        })?;
    Ok(Json(json!({ "bindings": bindings })))
}

pub async fn revoke_webhook(
    State(state): State<AppState>,
    Path(hook_id): Path<String>,
    request: axum::http::Request<axum::body::Body>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let principal_fp = extract_principal_fp(&request, &state);
    let remote_name = state.config.bind.to_string();
    let revoked = state
        .webhook_store()
        .revoke_binding(&hook_id, &principal_fp, &remote_name)
        .map_err(|err| {
            tracing::error!(error = %err, "internal error in webhook handler");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "internal server error" })),
            )
        })?;
    if !revoked {
        return Err((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "Webhook not found or already revoked" })),
        ));
    }
    Ok(Json(json!({ "revoked": hook_id })))
}

fn extract_principal_fp(
    request: &axum::http::Request<axum::body::Body>,
    state: &AppState,
) -> String {
    request
        .extensions()
        .get::<Principal>()
        .map(|p| p.fingerprint.clone())
        .unwrap_or_else(|| state.identity.principal_id())
}
