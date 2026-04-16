use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::policy;
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct PinRequest {
    pub name: String,
    pub hash: String,
}

pub async fn write_pin(
    State(state): State<AppState>,
    request: axum::http::Request<axum::body::Body>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let caller_scopes = policy::request_scopes(&request);
    policy::require_scope(&caller_scopes, "admin")?;

    let body_bytes = axum::body::to_bytes(request.into_body(), 1024 * 1024)
        .await
        .map_err(|_| (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(json!({ "error": "request body too large" })),
        ))?;
    let req: PinRequest = serde_json::from_slice(&body_bytes)
        .map_err(|err| (StatusCode::BAD_REQUEST, Json(json!({ "error": err.to_string() }))))?;

    state
        .refs_store()
        .write_pin(&req.name, &req.hash)
        .map_err(policy::internal_error)?;

    Ok(Json(json!({ "pinned": req.name, "hash": req.hash })))
}

pub async fn delete_pin(
    State(state): State<AppState>,
    Path(name): Path<String>,
    request: axum::http::Request<axum::body::Body>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let caller_scopes = policy::request_scopes(&request);
    policy::require_scope(&caller_scopes, "admin")?;

    let deleted = state
        .refs_store()
        .delete_pin(&name)
        .map_err(policy::internal_error)?;

    if !deleted {
        return Err((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("pin '{}' not found", name) })),
        ));
    }
    Ok(Json(json!({ "deleted": name })))
}

pub async fn list_pins(
    State(state): State<AppState>,
    request: axum::http::Request<axum::body::Body>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let caller_scopes = policy::request_scopes(&request);
    policy::require_scope(&caller_scopes, "admin")?;

    let pins = state
        .refs_store()
        .list_pins()
        .map_err(policy::internal_error)?;

    Ok(Json(json!({ "pins": pins })))
}
