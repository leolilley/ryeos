use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde_json::Value;

use crate::cas;
use crate::state::AppState;

pub async fn has_objects(
    State(state): State<AppState>,
    Json(req): Json<cas::HasObjectsRequest>,
) -> Json<Value> {
    let store = state.cas_store();
    let resp = store.handle_has(&req);
    Json(serde_json::to_value(resp).unwrap())
}

pub async fn put_objects(
    State(state): State<AppState>,
    Json(req): Json<cas::PutObjectsRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let store = state.cas_store();
    let resp = store.handle_put(&req);
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
    let resp = store.handle_get(&req);
    Json(serde_json::to_value(resp).unwrap())
}
