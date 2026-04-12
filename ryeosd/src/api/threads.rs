use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde_json::{json, Value};

use crate::policy;
use crate::state::AppState;

pub async fn list_threads(
    State(state): State<AppState>,
    request: axum::http::Request<axum::body::Body>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let caller_scopes = policy::request_scopes(&request);
    policy::require_scope(&caller_scopes, "threads.read")?;

    let limit: usize = request
        .uri()
        .query()
        .and_then(|q| q.split('&').find_map(|p| p.strip_prefix("limit=")))
        .and_then(|v| v.parse().ok())
        .unwrap_or(20);

    Ok(Json(
        state
            .threads
            .list_threads(limit)
            .map_err(policy::internal_error)?,
    ))
}

pub async fn get_thread(
    State(state): State<AppState>,
    Path(thread_id): Path<String>,
    request: axum::http::Request<axum::body::Body>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let caller_principal = policy::request_principal_id(&request, &state);
    let caller_scopes = policy::request_scopes(&request);

    let thread = state
        .threads
        .get_thread(&thread_id)
        .map_err(policy::internal_error)?;
    match thread {
        Some(thread) => {
            policy::check_thread_access(
                &caller_principal,
                &caller_scopes,
                thread.requested_by.as_deref(),
            )?;
            let facets = state.db.get_facets(&thread_id).map_err(policy::internal_error)?;
            let facets_map: std::collections::HashMap<&str, &str> =
                facets.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
            Ok(Json(json!({
                "thread": thread,
                "result": state.threads.get_thread_result(&thread_id).map_err(policy::internal_error)?,
                "artifacts": state.threads.list_thread_artifacts(&thread_id).map_err(policy::internal_error)?,
                "facets": facets_map,
            })))
        }
        None => Err((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("thread not found: {thread_id}") })),
        )),
    }
}

pub async fn list_children(
    State(state): State<AppState>,
    Path(thread_id): Path<String>,
    request: axum::http::Request<axum::body::Body>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let caller_principal = policy::request_principal_id(&request, &state);
    let caller_scopes = policy::request_scopes(&request);

    let thread = state
        .threads
        .get_thread(&thread_id)
        .map_err(policy::internal_error)?;
    match thread {
        Some(thread) => {
            policy::check_thread_access(
                &caller_principal,
                &caller_scopes,
                thread.requested_by.as_deref(),
            )?;
            Ok(Json(json!({
                "children": state.threads.list_children(&thread_id).map_err(policy::internal_error)?,
            })))
        }
        None => Err((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("thread not found: {thread_id}") })),
        )),
    }
}

pub async fn get_chain(
    State(state): State<AppState>,
    Path(thread_id): Path<String>,
    request: axum::http::Request<axum::body::Body>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let caller_principal = policy::request_principal_id(&request, &state);
    let caller_scopes = policy::request_scopes(&request);

    let thread = state
        .threads
        .get_thread(&thread_id)
        .map_err(policy::internal_error)?;
    match thread {
        Some(thread) => {
            policy::check_thread_access(
                &caller_principal,
                &caller_scopes,
                thread.requested_by.as_deref(),
            )?;
            let chain = state
                .threads
                .get_chain(&thread_id)
                .map_err(policy::internal_error)?;
            match chain {
                Some(chain) => Ok(Json(json!({
                    "threads": chain.threads,
                    "edges": chain.edges,
                }))),
                None => Err((
                    StatusCode::NOT_FOUND,
                    Json(json!({ "error": format!("thread not found: {thread_id}") })),
                )),
            }
        }
        None => Err((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("thread not found: {thread_id}") })),
        )),
    }
}
