use std::collections::HashMap;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct VaultSetRequest {
    pub principal_fp: String,
    pub name: String,
    pub envelope: Value,
}

#[derive(Debug, Deserialize)]
pub struct VaultDeleteRequest {
    pub principal_fp: String,
    pub name: String,
}

pub async fn vault_set(
    State(state): State<AppState>,
    Json(req): Json<VaultSetRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    state
        .vault_store()
        .set_secret(&req.principal_fp, &req.name, &req.envelope)
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
    Query(query): Query<HashMap<String, String>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let fp = query.get("principal_fp").ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "principal_fp required" })),
        )
    })?;
    let names = state.vault_store().list_secrets(fp).map_err(|err| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": err.to_string() })),
        )
    })?;
    Ok(Json(json!({ "names": names })))
}

pub async fn vault_delete(
    State(state): State<AppState>,
    Json(req): Json<VaultDeleteRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let deleted = state
        .vault_store()
        .delete_secret(&req.principal_fp, &req.name)
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
