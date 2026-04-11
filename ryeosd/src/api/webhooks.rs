use std::collections::HashMap;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct CreateWebhookRequest {
    pub principal_fp: String,
    pub item_id: String,
    pub project_path: String,
    pub description: Option<String>,
    pub secret_envelope: Option<Value>,
    pub vault_keys: Option<Vec<String>>,
}

pub async fn create_webhook(
    State(state): State<AppState>,
    Json(req): Json<CreateWebhookRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
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
            &req.principal_fp,
            &remote_name,
            &req.item_id,
            &req.project_path,
            req.description.as_deref(),
            req.secret_envelope.as_ref(),
            "",
            req.vault_keys.as_deref(),
        )
        .map_err(|err| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": err.to_string() })),
            )
        })?;

    Ok(Json(serde_json::to_value(result).unwrap()))
}

pub async fn list_webhooks(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let fp = query.get("principal_fp").ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "principal_fp required" })),
        )
    })?;
    let remote_name = state.config.bind.to_string();
    let bindings = state
        .webhook_store()
        .list_bindings(fp, &remote_name)
        .map_err(|err| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": err.to_string() })),
            )
        })?;
    Ok(Json(json!({ "bindings": bindings })))
}

pub async fn revoke_webhook(
    State(state): State<AppState>,
    Path(hook_id): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let fp = query.get("principal_fp").ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "principal_fp required" })),
        )
    })?;
    let remote_name = state.config.bind.to_string();
    let revoked = state
        .webhook_store()
        .revoke_binding(&hook_id, fp, &remote_name)
        .map_err(|err| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": err.to_string() })),
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
