use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::auth::Principal;
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct PushRequest {
    pub project_path: String,
    pub project_manifest_hash: String,
    #[allow(dead_code)]
    pub system_version: String,
    pub expected_snapshot_hash: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct PushUserSpaceRequest {
    pub user_manifest_hash: String,
    pub expected_revision: Option<i64>,
}

pub async fn push(
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
    let req: PushRequest = serde_json::from_slice(&body_bytes)
        .map_err(|err| invalid_request(err.into()))?;

    let refs = state.refs_store();

    let current_head = refs
        .resolve_project_ref(&principal_fp, &req.project_path)
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
            &principal_fp,
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
    request: axum::http::Request<axum::body::Body>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let principal_fp = extract_principal_fp(&request, &state);

    let body_bytes = axum::body::to_bytes(request.into_body(), 10 * 1024 * 1024)
        .await
        .map_err(|_| (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(json!({ "error": "request body too large" })),
        ))?;
    let req: PushUserSpaceRequest = serde_json::from_slice(&body_bytes)
        .map_err(|err| invalid_request(err.into()))?;

    let refs = state.refs_store();

    let result = refs
        .advance_user_space_ref(&principal_fp, &req.user_manifest_hash, req.expected_revision)
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
    request: axum::http::Request<axum::body::Body>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let fp = extract_principal_fp(&request, &state);

    let refs = state.refs_store();
    let result = refs.resolve_user_space_ref(&fp).map_err(internal_error)?;

    match result {
        Some(r) => Ok(Json(serde_json::to_value(r).unwrap())),
        None => Err((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "No user space pushed yet" })),
        )),
    }
}

/// Extract the authenticated principal fingerprint from request extensions.
/// Falls back to daemon identity when auth is not required.
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

fn invalid_request(err: anyhow::Error) -> (StatusCode, Json<Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": err.to_string() })),
    )
}

fn internal_error(err: anyhow::Error) -> (StatusCode, Json<Value>) {
    tracing::error!(error = %err, "internal error in push handler");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": "internal server error" })),
    )
}
