use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::policy;
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct WriteRefRequest {
    pub hash: String,
}

/// Validate a ref path: reject traversal, absolute paths, empty components.
fn validate_ref_path(ref_path: &str) -> Result<(), (StatusCode, Json<Value>)> {
    if ref_path.is_empty()
        || ref_path.contains("..")
        || ref_path.starts_with('/')
        || ref_path.contains('\0')
        || ref_path.contains('\\')
        || ref_path.split('/').any(|c| c.is_empty())
    {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "invalid ref path" })),
        ));
    }
    Ok(())
}

pub async fn get_ref(
    State(state): State<AppState>,
    Path(ref_path): Path<String>,
    request: axum::http::Request<axum::body::Body>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let caller_scopes = policy::request_scopes(&request);
    policy::require_scope(&caller_scopes, "admin")?;
    validate_ref_path(&ref_path)?;

    let hash = state
        .refs_store()
        .read_ref(&ref_path)
        .map_err(policy::internal_error)?;

    match hash {
        Some(h) => Ok(Json(json!({ "ref_path": ref_path, "hash": h }))),
        None => Err((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("ref '{}' not found", ref_path) })),
        )),
    }
}

pub async fn put_ref(
    State(state): State<AppState>,
    Path(ref_path): Path<String>,
    request: axum::http::Request<axum::body::Body>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let caller_scopes = policy::request_scopes(&request);
    policy::require_scope(&caller_scopes, "admin")?;
    validate_ref_path(&ref_path)?;

    let body_bytes = axum::body::to_bytes(request.into_body(), 1024 * 1024)
        .await
        .map_err(|_| (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(json!({ "error": "request body too large" })),
        ))?;
    let req: WriteRefRequest = serde_json::from_slice(&body_bytes)
        .map_err(|err| (StatusCode::BAD_REQUEST, Json(json!({ "error": err.to_string() }))))?;

    state
        .refs_store()
        .write_ref(&ref_path, &req.hash)
        .map_err(policy::internal_error)?;

    Ok(Json(json!({ "ref_path": ref_path, "hash": req.hash })))
}
