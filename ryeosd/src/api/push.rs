use std::collections::HashMap;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct PushRequest {
    pub project_path: String,
    pub project_manifest_hash: String,
    #[allow(dead_code)]
    pub system_version: String,
    pub expected_snapshot_hash: Option<String>,
    pub principal_fp: String,
}

#[derive(Debug, Deserialize)]
pub struct PushUserSpaceRequest {
    pub user_manifest_hash: String,
    pub expected_revision: Option<i64>,
    pub principal_fp: String,
}

pub async fn push(
    State(state): State<AppState>,
    Json(req): Json<PushRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let refs = state.refs_store();

    let current_head = refs
        .resolve_project_ref(&req.principal_fp, &req.project_path)
        .map_err(internal_error)?
        .map(|r| r.snapshot_hash);

    if req.expected_snapshot_hash.is_some() || current_head.is_some() {
        if req.expected_snapshot_hash.as_deref() != current_head.as_deref() {
            return Err((
                StatusCode::CONFLICT,
                Json(json!({
                    "error": "HEAD has moved",
                    "expected": req.expected_snapshot_hash,
                    "actual": current_head,
                })),
            ));
        }
    }

    let cas = state.cas_store();
    if cas
        .get_object(&req.project_manifest_hash)
        .map_err(internal_error)?
        .is_none()
    {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": format!("manifest {} not found in CAS", req.project_manifest_hash)
            })),
        ));
    }

    let now = chrono::Utc::now().to_rfc3339();
    let parent_hashes: Vec<&str> = current_head.as_deref().into_iter().collect();
    let snapshot = json!({
        "kind": "project_snapshot",
        "schema": 2,
        "project_manifest_hash": req.project_manifest_hash,
        "parent_hashes": parent_hashes,
        "source": "push",
        "timestamp": now,
    });
    let snapshot_hash = cas.store_object(&snapshot).map_err(internal_error)?;

    if !refs
        .advance_project_ref(
            &req.principal_fp,
            &req.project_path,
            &snapshot_hash,
            current_head.as_deref(),
        )
        .map_err(internal_error)?
    {
        return Err((
            StatusCode::CONFLICT,
            Json(json!({ "error": "HEAD moved during push" })),
        ));
    }

    Ok(Json(json!({
        "status": "ok",
        "project_path": req.project_path,
        "project_manifest_hash": req.project_manifest_hash,
        "snapshot_hash": snapshot_hash,
    })))
}

pub async fn push_user_space(
    State(state): State<AppState>,
    Json(req): Json<PushUserSpaceRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let refs = state.refs_store();

    let result = refs
        .advance_user_space_ref(&req.principal_fp, &req.user_manifest_hash, req.expected_revision)
        .map_err(|err| {
            let msg = err.to_string();
            if msg.contains("mismatch") || msg.contains("already exists") {
                (StatusCode::CONFLICT, Json(json!({ "error": msg })))
            } else if msg.contains("not found") {
                (StatusCode::NOT_FOUND, Json(json!({ "error": msg })))
            } else {
                internal_error(err)
            }
        })?;

    Ok(Json(json!({
        "status": "ok",
        "user_manifest_hash": result.user_manifest_hash,
        "revision": result.revision,
        "pushed_at": result.pushed_at,
    })))
}

pub async fn get_user_space(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let fp = query.get("principal_fp").ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "principal_fp required" })),
        )
    })?;

    let refs = state.refs_store();
    let result = refs.resolve_user_space_ref(fp).map_err(internal_error)?;

    match result {
        Some(r) => Ok(Json(serde_json::to_value(r).unwrap())),
        None => Err((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "No user space pushed yet" })),
        )),
    }
}

fn internal_error(err: anyhow::Error) -> (StatusCode, Json<Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": err.to_string() })),
    )
}
