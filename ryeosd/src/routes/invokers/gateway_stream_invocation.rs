//! Gateway stream invoker — body-driven launch + event tail.
//!
//! Parses `item_ref` / `project_path` / `parameters` from `input`,
//! mints a thread ID, spawns dispatch, subscribes to thread events,
//! and returns `RouteInvocationResult::Stream` with lag recovery.

use serde::Deserialize;
use serde_json::Value;

use crate::dispatch_error::RouteDispatchError;
use crate::routes::invocation::{
    CompiledRouteInvocation, PrincipalPolicy, RouteEventStream, RouteInvocationContract,
    RouteInvocationContext, RouteInvocationOutput, RouteInvocationResult,
};
use crate::services::event_store::EventReplayParams;

use super::stream_helpers::*;

pub struct CompiledGatewayStreamInvocation {
    pub keep_alive_secs: u64,
}

/// Typed body shape for gateway launch requests.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LaunchRequest {
    pub(crate) item_ref: String,
    pub(crate) project_path: String,
    pub(crate) parameters: Value,
}

static GATEWAY_CONTRACT: RouteInvocationContract = RouteInvocationContract {
    output: RouteInvocationOutput::Stream,
    principal: PrincipalPolicy::Optional,
};

#[axum::async_trait]
impl CompiledRouteInvocation for CompiledGatewayStreamInvocation {
    fn contract(&self) -> &'static RouteInvocationContract {
        &GATEWAY_CONTRACT
    }

    async fn invoke(
        &self,
        ctx: RouteInvocationContext,
    ) -> Result<RouteInvocationResult, RouteDispatchError> {
        // Gateway mints a new thread — Last-Event-ID is not meaningful.
        if ctx.headers.get("last-event-id").is_some() {
            return Err(RouteDispatchError::BadRequest(
                "Last-Event-ID is not supported on gateway endpoints".into(),
            ));
        }

        // Parse launch request from input (mode prepares it from body).
        let req: LaunchRequest = serde_json::from_value(ctx.input.clone()).map_err(|e| {
            RouteDispatchError::BadRequest(format!("invalid request body: {e}"))
        })?;

        let item_ref = crate::routes::parsed_ref::ParsedItemRef::parse(&req.item_ref).map_err(
            |e| {
                RouteDispatchError::BadRequest(format!(
                    "invalid item_ref '{}': {}",
                    req.item_ref, e
                ))
            },
        )?;

        let project_path =
            crate::routes::abs_path::AbsolutePathBuf::try_from_str(&req.project_path).map_err(
                |e| RouteDispatchError::BadRequest(format!("project_path: {e}")),
            )?;

        let thread_id = crate::services::thread_lifecycle::new_thread_id();

        let principal_id = ctx
            .principal
            .as_ref()
            .map(|p| p.id.clone())
            .unwrap_or_default();

        let principal_scopes = ctx
            .principal
            .as_ref()
            .map(|p| p.scopes.clone())
            .unwrap_or_default();

        let route_id: String = ctx.route_id.to_string();

        let span = tracing::info_span!(
            "dispatch_launch_sse",
            route_id = route_id.as_str(),
            thread_id = thread_id.as_str(),
            item_ref_kind = item_ref.kind(),
        );

        let hub = ctx.state.event_streams.clone();
        let mut rx = hub.subscribe(&thread_id);

        let mut launch_handle = crate::routes::launch::spawn_dispatch_launch(
            &ctx.state,
            item_ref,
            project_path,
            req.parameters,
            principal_id,
            principal_scopes,
            thread_id.clone(),
        );

        let events_svc = ctx.state.events.clone();
        let state_store_clone = ctx.state.state_store.clone();
        let thread_id_for_stream = thread_id.clone();
        let keep_alive_secs = self.keep_alive_secs;

        let stream = async_stream::stream! {
            let _guard = span.enter();
            yield Ok(
                axum::response::sse::Event::default()
                    .event("stream_started")
                    .data(serde_json::json!({"thread_id": thread_id_for_stream}).to_string())
            );

            let mut current_max: i64 = 0;
            let replay_batch_size = REPLAY_BATCH_SIZE;

            loop {
                tokio::select! {
                    recv_result = rx.recv() => {
                        match recv_result {
                            Ok(ev) => {
                                let event_type = ev.event_type.clone();
                                if ev.chain_seq > current_max {
                                    current_max = ev.chain_seq;
                                    yield Ok(
                                        axum::response::sse::Event::default()
                                            .event(ev.event_type.clone())
                                            .id(ev.chain_seq.to_string())
                                            .data(serde_json::to_string(&ev).expect("PersistedEventRecord serializes"))
                                    );
                                }
                                if is_terminal(&event_type) {
                                    return;
                                }
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                                let mut lag_max = current_max;
                                let mut lag_error: Option<String> = None;
                                let mut next_after: Option<i64> =
                                    if current_max > 0 { Some(current_max) } else { None };
                                loop {
                                    let page = events_svc.replay(&EventReplayParams {
                                        chain_root_id: None,
                                        thread_id: Some(thread_id_for_stream.clone()),
                                        after_chain_seq: next_after,
                                        limit: replay_batch_size,
                                    });
                                    match page {
                                        Ok(page_result) => {
                                            if page_result.events.is_empty() {
                                                break;
                                            }
                                            for ev in &page_result.events {
                                                if ev.chain_seq > lag_max {
                                                    lag_max = ev.chain_seq;
                                                    yield Ok(
                                                        axum::response::sse::Event::default()
                                                            .event(ev.event_type.clone())
                                                            .id(ev.chain_seq.to_string())
                                                            .data(serde_json::to_string(&ev).expect("PersistedEventRecord serializes"))
                                                    );
                                                    if is_terminal(&ev.event_type) {
                                                        return;
                                                    }
                                                }
                                            }
                                            if page_result.next_cursor.is_none() {
                                                break;
                                            }
                                            next_after = Some(lag_max);
                                        }
                                        Err(e) => {
                                            lag_error = Some(format!("lag replay failed: {e}"));
                                            break;
                                        }
                                    }
                                }

                                if let Some(err_msg) = lag_error {
                                    let thread = state_store_clone.get_thread(&thread_id_for_stream);
                                    if let Ok(Some(detail)) = thread {
                                        if is_terminal_status(&detail.status) {
                                            return;
                                        }
                                    }
                                    yield Ok(sse_error_event("replay_failed", &err_msg));
                                    return;
                                }

                                current_max = lag_max;

                                tracing::info!(
                                    thread_id = %thread_id_for_stream,
                                    lagged = n,
                                    "dispatch_launch SSE subscriber lag recovery complete"
                                );
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                return;
                            }
                        }
                    }
                    join_result = &mut launch_handle => {
                        match join_result {
                            Ok(Ok(())) => {
                                // Post-launch drain: replay any events the broadcast
                                // didn't deliver from the durable store.
                                let mut next_after: Option<i64> =
                                    if current_max > 0 { Some(current_max) } else { None };
                                let mut saw_terminal = false;
                                loop {
                                    let page = events_svc.replay(&EventReplayParams {
                                        chain_root_id: None,
                                        thread_id: Some(thread_id_for_stream.clone()),
                                        after_chain_seq: next_after,
                                        limit: replay_batch_size,
                                    });
                                    match page {
                                        Ok(page_result) => {
                                            if page_result.events.is_empty() {
                                                break;
                                            }
                                            for ev in &page_result.events {
                                                if ev.chain_seq > current_max {
                                                    current_max = ev.chain_seq;
                                                    yield Ok(
                                                        axum::response::sse::Event::default()
                                                            .event(ev.event_type.clone())
                                                            .id(ev.chain_seq.to_string())
                                                            .data(serde_json::to_string(&ev).expect("PersistedEventRecord serializes"))
                                                    );
                                                    if is_terminal(&ev.event_type) {
                                                        saw_terminal = true;
                                                        break;
                                                    }
                                                }
                                            }
                                            if saw_terminal {
                                                return;
                                            }
                                            if page_result.next_cursor.is_none() {
                                                break;
                                            }
                                            next_after = Some(current_max);
                                        }
                                        Err(e) => {
                                            yield Ok(sse_error_event("post_launch_replay_failed", &format!("post-launch replay failed: {e}")));
                                            return;
                                        }
                                    }
                                }
                                let detail = state_store_clone.get_thread(&thread_id_for_stream);
                                if let Ok(Some(d)) = detail {
                                    if is_terminal_status(&d.status) {
                                        return;
                                    }
                                }
                                yield Ok(sse_error_event("thread_not_terminal", "launch completed but thread is not terminal"));
                                return;
                            }
                            Ok(Err(e)) => {
                                let extras = match &e {
                                    crate::routes::launch::LaunchSpawnError::Dispatch(de) => {
                                        let payload = crate::structured_error::StructuredErrorPayload::from(de);
                                        // Strip `code` and `error` so the SSE helper's explicit args win.
                                        let mut value = payload.to_value();
                                        if let Some(map) = value.as_object_mut() {
                                            map.remove("code");
                                            map.remove("error");
                                        }
                                        Some(value)
                                    }
                                    _ => None,
                                };
                                yield Ok(sse_error_event_with(
                                    e.code(),
                                    &format!("launch failed: {e}"),
                                    extras,
                                ));
                                return;
                            }
                            Err(_) => {
                                yield Ok(sse_error_event("task_panicked", "launch task panicked"));
                                return;
                            }
                        }
                    }
                }
            }
        };

        Ok(RouteInvocationResult::Stream(RouteEventStream {
            events: Box::pin(stream),
            keep_alive_secs,
        }))
    }
}
