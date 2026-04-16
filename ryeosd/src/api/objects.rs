use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde_json::{json, Value};

use crate::cas;
use crate::policy;
use crate::state::AppState;

pub async fn has_objects(
    State(state): State<AppState>,
    Json(req): Json<cas::HasObjectsRequest>,
) -> Json<Value> {
    let store = state.cas_store();
    let resp = cas::handle_has(store, &req);
    Json(serde_json::to_value(resp).unwrap())
}

pub async fn put_objects(
    State(state): State<AppState>,
    request: axum::http::Request<axum::body::Body>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let _ = policy::request_principal_id(&request, &state);
    let caller_scopes = policy::request_scopes(&request);
    policy::require_scope(&caller_scopes, "objects")?;

    let body_bytes = axum::body::to_bytes(request.into_body(), 10 * 1024 * 1024)
        .await
        .map_err(|_| (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(json!({ "error": "request body too large" })),
        ))?;
    let req: cas::PutObjectsRequest = serde_json::from_slice(&body_bytes)
        .map_err(|err| (StatusCode::BAD_REQUEST, Json(json!({ "error": err.to_string() }))))?;

    let store = state.cas_store();
    let resp = cas::handle_put(store, &req);
    if !resp.errors.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::to_value(&resp).unwrap()),
        ));
    }
    Ok(Json(serde_json::to_value(&resp).unwrap()))
}

pub async fn get_objects(
    State(state): State<AppState>,
    Json(req): Json<cas::GetObjectsRequest>,
) -> Json<Value> {
    let store = state.cas_store();
    let resp = cas::handle_get(store, &req);
    Json(serde_json::to_value(resp).unwrap())
}
