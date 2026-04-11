use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct ListThreadsQuery {
    #[serde(default = "default_limit")]
    limit: usize,
}

fn default_limit() -> usize {
    20
}

pub async fn list_threads(
    State(state): State<AppState>,
    Query(query): Query<ListThreadsQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    Ok(Json(
        state
            .threads
            .list_threads(query.limit)
            .map_err(internal_error)?,
    ))
}

pub async fn get_thread(
    State(state): State<AppState>,
    Path(thread_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let thread = state
        .threads
        .get_thread(&thread_id)
        .map_err(internal_error)?;
    match thread {
        Some(thread) => Ok(Json(json!({
            "thread": thread,
            "result": state.threads.get_thread_result(&thread_id).map_err(internal_error)?,
            "artifacts": state.threads.list_thread_artifacts(&thread_id).map_err(internal_error)?,
        }))),
        None => Err((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("thread not found: {thread_id}") })),
        )),
    }
}

pub async fn list_children(
    State(state): State<AppState>,
    Path(thread_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if state
        .threads
        .get_thread(&thread_id)
        .map_err(internal_error)?
        .is_none()
    {
        return Err((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("thread not found: {thread_id}") })),
        ));
    }

    Ok(Json(json!({
        "children": state.threads.list_children(&thread_id).map_err(internal_error)?,
    })))
}

pub async fn get_chain(
    State(state): State<AppState>,
    Path(thread_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    match state
        .threads
        .get_chain(&thread_id)
        .map_err(internal_error)?
    {
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

fn internal_error(err: anyhow::Error) -> (StatusCode, Json<Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": err.to_string() })),
    )
}
