use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::auth::Principal;
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct VaultSetRequest {
    pub name: String,
    pub envelope: Value,
}

#[derive(Debug, Deserialize)]
pub struct VaultDeleteRequest {
    pub name: String,
}

pub async fn vault_set(
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
    let req: VaultSetRequest = serde_json::from_slice(&body_bytes)
        .map_err(|err| (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": err.to_string() })),
        ))?;

    state
        .vault_store()
        .set_secret(&principal_fp, &req.name, &req.envelope)
        .map_err(|err| {
            (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": err.to_string() })),
            )
        })?;
    Ok(Json(json!({ "stored": req.name })))
}

pub async fn vault_list(
    State(state): State<AppState>,
    request: axum::http::Request<axum::body::Body>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let principal_fp = extract_principal_fp(&request, &state);

    let names = state.vault_store().list_secrets(&principal_fp).map_err(|err| {
        tracing::error!(error = %err, "internal error in vault handler");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "internal server error" })),
        )
    })?;
    Ok(Json(json!({ "names": names })))
}

pub async fn vault_delete(
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
    let req: VaultDeleteRequest = serde_json::from_slice(&body_bytes)
        .map_err(|err| (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": err.to_string() })),
        ))?;

    let deleted = state
        .vault_store()
        .delete_secret(&principal_fp, &req.name)
        .map_err(|err| {
            (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": err.to_string() })),
            )
        })?;
    if !deleted {
        return Err((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("Secret '{}' not found", req.name) })),
        ));
    }
    Ok(Json(json!({ "deleted": req.name })))
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
