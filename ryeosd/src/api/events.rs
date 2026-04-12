use std::convert::Infallible;
use std::sync::Arc;

use async_stream::stream;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::broadcast::error::TryRecvError;

use crate::policy;
use crate::services::event_store::EventReplayParams;
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct EventReplayQuery {
    #[serde(default)]
    after: Option<i64>,
    #[serde(default = "default_limit")]
    limit: usize,
}

fn default_limit() -> usize {
    200
}

pub async fn get_thread_events(
    State(state): State<AppState>,
    Path(thread_id): Path<String>,
    Query(query): Query<EventReplayQuery>,
    request: axum::http::Request<axum::body::Body>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let caller_principal = policy::request_principal_id(&request, &state);
    let caller_scopes = policy::request_scopes(&request);

    let thread = state
        .threads
        .get_thread(&thread_id)
        .map_err(policy::internal_error)?
        .ok_or_else(|| not_found(format!("thread not found: {thread_id}")))?;
    policy::check_thread_access(&caller_principal, &caller_scopes, thread.requested_by.as_deref())?;

    let result = state
        .events
        .replay(&EventReplayParams {
            chain_root_id: None,
            thread_id: Some(thread_id),
            after_chain_seq: query.after,
            limit: query.limit,
        })
        .map_err(policy::internal_error)?;
    Ok(Json(json!({
        "events": result.events,
        "next_cursor": result.next_cursor,
    })))
}

pub async fn get_chain_events(
    State(state): State<AppState>,
    Path(chain_root_id): Path<String>,
    Query(query): Query<EventReplayQuery>,
    request: axum::http::Request<axum::body::Body>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let caller_principal = policy::request_principal_id(&request, &state);
    let caller_scopes = policy::request_scopes(&request);

    let root_thread = state
        .threads
        .get_thread(&chain_root_id)
        .map_err(policy::internal_error)?
        .ok_or_else(|| not_found(format!("chain not found: {chain_root_id}")))?;
    policy::check_thread_access(&caller_principal, &caller_scopes, root_thread.requested_by.as_deref())?;

    let result = state
        .events
        .replay(&EventReplayParams {
            chain_root_id: Some(chain_root_id),
            thread_id: None,
            after_chain_seq: query.after,
            limit: query.limit,
        })
        .map_err(policy::internal_error)?;
    Ok(Json(json!({
        "events": result.events,
        "next_cursor": result.next_cursor,
    })))
}

pub async fn stream_thread_events(
    State(state): State<AppState>,
    Path(thread_id): Path<String>,
    Query(query): Query<EventReplayQuery>,
    request: axum::http::Request<axum::body::Body>,
) -> Result<
    Sse<impl futures_core::Stream<Item = Result<Event, Infallible>>>,
    (StatusCode, Json<Value>),
> {
    let caller_principal = policy::request_principal_id(&request, &state);
    let caller_scopes = policy::request_scopes(&request);

    let thread = state
        .threads
        .get_thread(&thread_id)
        .map_err(policy::internal_error)?
        .ok_or_else(|| not_found(format!("thread not found: {thread_id}")))?;
    policy::check_thread_access(&caller_principal, &caller_scopes, thread.requested_by.as_deref())?;

    Ok(build_event_stream(
        state.events.clone(),
        thread.chain_root_id,
        Some(thread.thread_id),
        query.after,
        query.limit,
    ))
}

pub async fn stream_chain_events(
    State(state): State<AppState>,
    Path(chain_root_id): Path<String>,
    Query(query): Query<EventReplayQuery>,
    request: axum::http::Request<axum::body::Body>,
) -> Result<
    Sse<impl futures_core::Stream<Item = Result<Event, Infallible>>>,
    (StatusCode, Json<Value>),
> {
    let caller_principal = policy::request_principal_id(&request, &state);
    let caller_scopes = policy::request_scopes(&request);

    let root_thread = state
        .threads
        .get_thread(&chain_root_id)
        .map_err(policy::internal_error)?
        .ok_or_else(|| not_found(format!("chain not found: {chain_root_id}")))?;
    policy::check_thread_access(&caller_principal, &caller_scopes, root_thread.requested_by.as_deref())?;

    Ok(build_event_stream(
        state.events.clone(),
        chain_root_id,
        None,
        query.after,
        query.limit,
    ))
}

fn build_event_stream(
    events: Arc<crate::services::event_store::EventStoreService>,
    chain_root_id: String,
    thread_id: Option<String>,
    after_chain_seq: Option<i64>,
    batch_size: usize,
) -> Sse<impl futures_core::Stream<Item = Result<Event, Infallible>>> {
    let replay_batch_size = batch_size.max(1);
    let stream = stream! {
        let mut receiver = events.subscribe();
        let mut last_chain_seq = after_chain_seq.unwrap_or(0);

        match emit_indexed_backlog(
            &events,
            &chain_root_id,
            thread_id.as_deref(),
            &mut last_chain_seq,
            replay_batch_size,
        ) {
            Ok(backlog) => {
                for event in backlog {
                    yield Ok(as_sse_event(&event));
                }
            }
            Err(err) => {
                yield Ok(error_event("replay_failed", err.to_string()));
                return;
            }
        }

        loop {
            match receiver.try_recv() {
                Ok(event) => {
                    if matches_stream(&event, &chain_root_id, thread_id.as_deref(), last_chain_seq) {
                        last_chain_seq = event.chain_seq;
                        yield Ok(as_sse_event(&event));
                    }
                    continue;
                }
                Err(TryRecvError::Lagged(_)) => {
                    match emit_live_catch_up(
                        &events,
                        &chain_root_id,
                        thread_id.as_deref(),
                        &mut last_chain_seq,
                        replay_batch_size,
                    ) {
                        Ok(caught_up) => {
                            for event in caught_up {
                                yield Ok(as_sse_event(&event));
                            }
                        }
                        Err(err) => {
                            yield Ok(error_event("live_catch_up_failed", err.to_string()));
                            return;
                        }
                    }
                    continue;
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Closed) => return,
            }

            match receiver.recv().await {
                Ok(event) => {
                    if matches_stream(&event, &chain_root_id, thread_id.as_deref(), last_chain_seq) {
                        last_chain_seq = event.chain_seq;
                        yield Ok(as_sse_event(&event));
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    match emit_live_catch_up(
                        &events,
                        &chain_root_id,
                        thread_id.as_deref(),
                        &mut last_chain_seq,
                        replay_batch_size,
                    ) {
                        Ok(caught_up) => {
                            for event in caught_up {
                                yield Ok(as_sse_event(&event));
                            }
                        }
                        Err(err) => {
                            yield Ok(error_event("live_catch_up_failed", err.to_string()));
                            return;
                        }
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            }
        }
    };

    Sse::new(stream).keep_alive(KeepAlive::default())
}

fn emit_indexed_backlog(
    events: &crate::services::event_store::EventStoreService,
    chain_root_id: &str,
    thread_id: Option<&str>,
    last_chain_seq: &mut i64,
    batch_size: usize,
) -> anyhow::Result<Vec<crate::db::PersistedEventRecord>> {
    let mut backlog = Vec::new();

    loop {
        let replay = events.replay(&EventReplayParams {
            chain_root_id: Some(chain_root_id.to_string()),
            thread_id: thread_id.map(str::to_string),
            after_chain_seq: Some(*last_chain_seq),
            limit: batch_size,
        })?;
        if replay.events.is_empty() {
            break;
        }
        *last_chain_seq = replay
            .events
            .last()
            .map(|event| event.chain_seq)
            .unwrap_or(*last_chain_seq);
        let drained = replay.events.len() < batch_size;
        backlog.extend(replay.events);
        if drained {
            break;
        }
    }

    Ok(backlog)
}

fn emit_live_catch_up(
    events: &crate::services::event_store::EventStoreService,
    chain_root_id: &str,
    thread_id: Option<&str>,
    last_chain_seq: &mut i64,
    batch_size: usize,
) -> anyhow::Result<Vec<crate::db::PersistedEventRecord>> {
    let mut catch_up = Vec::new();

    loop {
        let batch =
            events.live_catch_up(chain_root_id, thread_id, Some(*last_chain_seq), batch_size)?;
        if batch.is_empty() {
            break;
        }
        *last_chain_seq = batch
            .last()
            .map(|event| event.chain_seq)
            .unwrap_or(*last_chain_seq);
        let drained = batch.len() < batch_size;
        catch_up.extend(batch);
        if drained {
            break;
        }
    }

    Ok(catch_up)
}

fn matches_stream(
    event: &crate::db::PersistedEventRecord,
    chain_root_id: &str,
    thread_id: Option<&str>,
    last_chain_seq: i64,
) -> bool {
    event.chain_root_id == chain_root_id
        && event.chain_seq > last_chain_seq
        && thread_id.map_or(true, |thread_id| event.thread_id == thread_id)
}

fn as_sse_event(event: &crate::db::PersistedEventRecord) -> Event {
    match serde_json::to_string(event) {
        Ok(data) => Event::default()
            .id(event.chain_seq.to_string())
            .event(event.event_type.clone())
            .data(data),
        Err(err) => error_event("encode_failed", err.to_string()),
    }
}

fn error_event(code: &str, message: String) -> Event {
    tracing::error!(code = code, error = %message, "SSE stream error");
    Event::default().event("error").data(
        json!({
            "code": code,
        })
        .to_string(),
    )
}

fn not_found(message: String) -> (StatusCode, Json<Value>) {
    (StatusCode::NOT_FOUND, Json(json!({ "error": message })))
}
