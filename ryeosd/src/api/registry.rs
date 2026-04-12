use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::policy;
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct PublishRequest {
    pub kind: String,
    pub item_id: String,
    pub version: String,
    pub manifest_hash: String,
}

#[derive(Debug, Deserialize)]
pub struct ClaimNamespaceRequest {
    pub namespace: String,
}

#[derive(Debug, Deserialize)]
pub struct RegisterIdentityRequest {
    pub identity: Value,
}

#[derive(Debug, Deserialize)]
pub struct SearchQuery {
    pub query: Option<String>,
    pub kind: Option<String>,
    pub namespace: Option<String>,
    pub limit: Option<usize>,
}

pub async fn publish(
    State(state): State<AppState>,
    request: axum::http::Request<axum::body::Body>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let publisher_fp = policy::request_principal_id(&request, &state);
    let caller_scopes = policy::request_scopes(&request);
    policy::require_scope(&caller_scopes, "registry.publish")?;

    let body_bytes = axum::body::to_bytes(request.into_body(), 10 * 1024 * 1024)
        .await
        .map_err(|_| (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(json!({ "error": "request body too large" })),
        ))?;
    let req: PublishRequest = serde_json::from_slice(&body_bytes)
        .map_err(|err| (StatusCode::BAD_REQUEST, Json(json!({ "error": err.to_string() }))))?;

    let result = state
        .registry_store()
        .publish_item(
            &req.kind,
            &req.item_id,
            &req.version,
            &req.manifest_hash,
            &publisher_fp,
        )
        .map_err(policy::internal_error)?;
    if result.get("ok").and_then(|v| v.as_bool()) != Some(true) {
        return Err((StatusCode::BAD_REQUEST, Json(result)));
    }
    Ok(Json(result))
}

pub async fn search(
    State(state): State<AppState>,
    Query(q): Query<SearchQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let limit = q.limit.unwrap_or(20).min(100);
    let results = state
        .registry_store()
        .search_items(
            q.query.as_deref(),
            q.kind.as_deref(),
            q.namespace.as_deref(),
            limit,
        )
        .map_err(policy::internal_error)?;
    Ok(Json(json!({ "results": results, "total": results.len() })))
}

pub async fn get_item(
    State(state): State<AppState>,
    Path((kind, item_id)): Path<(String, String)>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let item = state
        .registry_store()
        .get_item(&kind, &item_id)
        .map_err(policy::internal_error)?;
    match item {
        Some(v) => Ok(Json(v)),
        None => Err((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("Item not found: {kind}/{item_id}") })),
        )),
    }
}

pub async fn get_version(
    State(state): State<AppState>,
    Path((kind, item_id, version)): Path<(String, String, String)>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let ver = state
        .registry_store()
        .get_version(&kind, &item_id, &version)
        .map_err(policy::internal_error)?;
    match ver {
        Some(v) => Ok(Json(v)),
        None => Err((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("Version not found: {kind}/{item_id}@{version}") })),
        )),
    }
}

pub async fn claim_namespace(
    State(state): State<AppState>,
    request: axum::http::Request<axum::body::Body>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let owner_fp = policy::request_principal_id(&request, &state);
    let caller_scopes = policy::request_scopes(&request);
    policy::require_scope(&caller_scopes, "registry.publish")?;

    let body_bytes = axum::body::to_bytes(request.into_body(), 10 * 1024 * 1024)
        .await
        .map_err(|_| (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(json!({ "error": "request body too large" })),
        ))?;
    let req: ClaimNamespaceRequest = serde_json::from_slice(&body_bytes)
        .map_err(|err| (StatusCode::BAD_REQUEST, Json(json!({ "error": err.to_string() }))))?;

    policy::validate_path_component(&req.namespace)?;

    let result = state
        .registry_store()
        .claim_namespace(&req.namespace, &owner_fp)
        .map_err(policy::internal_error)?;
    if result.get("ok").and_then(|v| v.as_bool()) != Some(true) {
        return Err((StatusCode::BAD_REQUEST, Json(result)));
    }
    Ok(Json(result))
}

pub async fn register_identity(
    State(state): State<AppState>,
    Json(req): Json<RegisterIdentityRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let result = state
        .registry_store()
        .register_identity(&req.identity)
        .map_err(policy::internal_error)?;
    if result.get("ok").and_then(|v| v.as_bool()) != Some(true) {
        return Err((StatusCode::BAD_REQUEST, Json(result)));
    }
    Ok(Json(result))
}

pub async fn lookup_identity(
    State(state): State<AppState>,
    Path(fingerprint): Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let doc = state
        .registry_store()
        .lookup_identity(&fingerprint)
        .map_err(policy::internal_error)?;
    match doc {
        Some(v) => Ok(Json(v)),
        None => Err((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("Identity not found: {fingerprint}") })),
        )),
    }
}
