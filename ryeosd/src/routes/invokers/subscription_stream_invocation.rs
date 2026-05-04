//! Subscription stream invoker — path-driven event tail for existing threads.
//!
//! Extracts `thread_id` from `input`, checks principal ownership,
//! subscribes to thread events, and returns `RouteInvocationResult::Stream`
//! with initial replay and lag recovery.

use crate::dispatch_error::RouteDispatchError;
use crate::routes::invocation::{
    CompiledRouteInvocation, PrincipalPolicy, RouteEventStream, RouteInvocationContract,
    RouteInvocationContext, RouteInvocationOutput, RouteInvocationResult,
};
use crate::services::event_store::EventReplayParams;

use super::stream_helpers::*;

pub struct CompiledSubscriptionStreamInvocation {
    pub keep_alive_secs: u64,
}

static SUBSCRIPTION_CONTRACT: RouteInvocationContract = RouteInvocationContract {
    output: RouteInvocationOutput::Stream,
    principal: PrincipalPolicy::Required,
};

#[axum::async_trait]
impl CompiledRouteInvocation for CompiledSubscriptionStreamInvocation {
    fn contract(&self) -> &'static RouteInvocationContract {
        &SUBSCRIPTION_CONTRACT
    }

    async fn invoke(
        &self,
        ctx: RouteInvocationContext,
    ) -> Result<RouteInvocationResult, RouteDispatchError> {
        // Thread ID comes from input (mode interpolates from captures).
        let thread_id = ctx
            .input
            .get("thread_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                RouteDispatchError::BadRequest("missing thread_id in request".into())
            })?
            .to_string();

        // Last-Event-ID for resumption.
        let last_event_id = parse_last_event_id(&ctx.headers)?;

        // Ownership check: only the thread creator can subscribe.
        let principal = ctx
            .principal
            .as_ref()
            .ok_or(RouteDispatchError::Unauthorized)?;

        let thread_detail = ctx
            .state
            .state_store
            .get_thread(&thread_id)
            .map_err(|e| RouteDispatchError::Internal(e.to_string()))?
            .ok_or(RouteDispatchError::NotFound)?;

        let requested_by = match &thread_detail.requested_by {
            Some(r) => r,
            None => return Err(RouteDispatchError::NotFound),
        };

        if principal.id != *requested_by {
            return Err(RouteDispatchError::NotFound);
        }

        let hub = ctx.state.event_streams.clone();
        let mut rx = hub.subscribe(&thread_id);

        let last_seen = last_event_id;

        let events_svc = ctx.state.events.clone();
        let state_store_clone = ctx.state.state_store.clone();
        let keep_alive_secs = self.keep_alive_secs;

        let stream = async_stream::stream! {
            yield Ok(
                axum::response::sse::Event::default()
                    .event("stream_started")
                    .data(serde_json::json!({"thread_id": thread_id}).to_string())
            );

            let replay_batch_size = REPLAY_BATCH_SIZE;

            let replay_result = events_svc
                .replay(&EventReplayParams {
                    chain_root_id: None,
                    thread_id: Some(thread_id.clone()),
                    after_chain_seq: last_seen,
                    limit: replay_batch_size,
                })
                .map_err(|e| RouteDispatchError::Internal(e.to_string()));

            match replay_result {
                Ok(result) => {
                    let mut max_seq = last_seen.unwrap_or(0);
                    let mut saw_terminal = false;
                    let mut yielded_any_replay_event = false;

                    for ev in &result.events {
                        max_seq = max_seq.max(ev.chain_seq);
                        yield Ok(sse_event_for_persisted(ev));
                        yielded_any_replay_event = true;
                        if is_terminal(&ev.event_type) {
                            saw_terminal = true;
                        }
                    }

                    if saw_terminal {
                        return;
                    }

                    let mut cursor = result.next_cursor;
                    while let Some(_c) = cursor {
                        let page = events_svc
                            .replay(&EventReplayParams {
                                chain_root_id: None,
                                thread_id: Some(thread_id.clone()),
                                after_chain_seq: Some(max_seq),
                                limit: replay_batch_size,
                            });

                        match page {
                            Ok(page_result) => {
                                if page_result.events.is_empty() {
                                    break;
                                }
                                for ev in &page_result.events {
                                    max_seq = max_seq.max(ev.chain_seq);
                                    yield Ok(sse_event_for_persisted(ev));
                                    yielded_any_replay_event = true;
                                    if is_terminal(&ev.event_type) {
                                        return;
                                    }
                                }
                                cursor = page_result.next_cursor;
                            }
                            Err(e) => {
                                yield Ok(sse_error_event("replay_paging_failed", &format!("replay paging failed: {e}")));
                                return;
                            }
                        }
                    }

                    if !yielded_any_replay_event {
                        let detail = state_store_clone.get_thread(&thread_id);
                        if let Ok(Some(d)) = &detail {
                            if is_terminal_status(&d.status) {
                                return;
                            }
                        }
                    }

                    let mut current_max = max_seq;
                    loop {
                        match rx.recv().await {
                            Ok(ev) => {
                                if ev.chain_seq <= current_max {
                                    continue;
                                }
                                current_max = ev.chain_seq;
                                yield Ok(sse_event_for_persisted(&ev));
                                if is_terminal(&ev.event_type) {
                                    return;
                                }
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                                let lag_cursor = current_max;
                                let mut lag_max = current_max;
                                let mut lag_error: Option<String> = None;

                                loop {
                                    let page_result = events_svc
                                        .replay(&EventReplayParams {
                                            chain_root_id: None,
                                            thread_id: Some(thread_id.clone()),
                                            after_chain_seq: if lag_max > lag_cursor {
                                                Some(lag_max)
                                            } else {
                                                Some(lag_cursor)
                                            },
                                            limit: replay_batch_size,
                                        });

                                    match page_result {
                                        Ok(page) => {
                                            if page.events.is_empty() {
                                                break;
                                            }
                                            for ev in &page.events {
                                                lag_max = lag_max.max(ev.chain_seq);
                                                if ev.chain_seq > current_max {
                                                    yield Ok(sse_event_for_persisted(ev));
                                                    if is_terminal(&ev.event_type) {
                                                        return;
                                                    }
                                                }
                                            }
                                            if page.next_cursor.is_none() {
                                                break;
                                            }
                                        }
                                        Err(e) => {
                                            lag_error = Some(format!("replay failed: {e}"));
                                            break;
                                        }
                                    }
                                }

                                if let Some(err_msg) = lag_error {
                                    let thread = state_store_clone.get_thread(&thread_id);
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
                                    thread_id = %thread_id,
                                    lagged = n,
                                    "SSE subscriber lag recovery complete"
                                );
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                return;
                            }
                        }
                    }
                }
                Err(e) => {
                    yield Ok(sse_error_event("initial_replay_failed", &format!("replay failed: {e}")));
                    return;
                }
            }
        };

        Ok(RouteInvocationResult::Stream(RouteEventStream {
            events: Box::pin(stream),
            keep_alive_secs,
        }))
    }
}

/// Parse `Last-Event-ID` header for SSE resumption.
pub(crate) fn parse_last_event_id(
    headers: &axum::http::HeaderMap,
) -> Result<Option<i64>, RouteDispatchError> {
    let raw = match headers.get("last-event-id") {
        Some(v) => v,
        None => return Ok(None),
    };

    let s = raw.to_str().map_err(|_| RouteDispatchError::BadLastEventId)?;
    let n = s
        .parse::<i64>()
        .map_err(|_| RouteDispatchError::BadLastEventId)?;

    if n < 0 {
        return Err(RouteDispatchError::BadLastEventId);
    }

    Ok(Some(n))
}
