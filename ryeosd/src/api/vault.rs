use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::policy;
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
    let principal_fp = policy::request_principal_id(&request, &state);
    let caller_scopes = policy::request_scopes(&request);
    policy::require_scope(&caller_scopes, "vault")?;

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
        .map_err(|err| policy::internal_error(err))?;
    Ok(Json(json!({ "stored": req.name })))
}

pub async fn vault_get(
    State(state): State<AppState>,
    request: axum::http::Request<axum::body::Body>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let principal_fp = policy::request_principal_id(&request, &state);
    let caller_scopes = policy::request_scopes(&request);
    policy::require_scope(&caller_scopes, "vault")?;

    let body_bytes = axum::body::to_bytes(request.into_body(), 1024 * 1024)
        .await
        .map_err(|_| (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(json!({ "error": "request body too large" })),
        ))?;

    #[derive(Deserialize)]
    struct VaultGetRequest {
        name: String,
    }
    let req: VaultGetRequest = serde_json::from_slice(&body_bytes)
        .map_err(|err| (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": err.to_string() })),
        ))?;

    let envelope = state
        .vault_store()
        .get_secret(&principal_fp, &req.name)
        .map_err(policy::internal_error)?;

    match envelope {
        Some(env) => Ok(Json(json!({ "name": req.name, "envelope": env }))),
        None => Err((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("Secret '{}' not found", req.name) })),
        )),
    }
}

pub async fn vault_list(
    State(state): State<AppState>,
    request: axum::http::Request<axum::body::Body>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let principal_fp = policy::request_principal_id(&request, &state);
    let caller_scopes = policy::request_scopes(&request);
    policy::require_scope(&caller_scopes, "vault")?;

    let names = state
        .vault_store()
        .list_secrets(&principal_fp)
        .map_err(policy::internal_error)?;
    Ok(Json(json!({ "names": names })))
}

pub async fn vault_delete(
    State(state): State<AppState>,
    request: axum::http::Request<axum::body::Body>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let principal_fp = policy::request_principal_id(&request, &state);
    let caller_scopes = policy::request_scopes(&request);
    policy::require_scope(&caller_scopes, "vault")?;

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
        .map_err(|err| policy::internal_error(err))?;
    if !deleted {
        return Err((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("Secret '{}' not found", req.name) })),
        ));
    }
    Ok(Json(json!({ "deleted": req.name })))
}
