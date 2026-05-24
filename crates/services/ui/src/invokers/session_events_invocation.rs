//! `session_events` invoker — subscribes to a session's event bus.
//!
//! Reads from `SessionBus::subscribe(session_id)` and yields SSE frames.
//! The browser's `/ui/events/session/{id}` route uses this invoker.

use ryeos_api::route_error::RouteDispatchError;
use ryeos_api::routes::invocation::{
    CompiledRouteInvocation, PrincipalPolicy, RouteEventStream, RouteInvocationContext,
    RouteInvocationContract, RouteInvocationOutput, RouteInvocationResult,
};
use tokio_stream::Stream;

use ryeos_app::stream_envelope::RouteStreamEnvelope;

pub struct CompiledSessionEventsInvocation {
    pub keep_alive_secs: u64,
}

static SESSION_EVENTS_CONTRACT: RouteInvocationContract = RouteInvocationContract {
    output: RouteInvocationOutput::Stream,
    principal: PrincipalPolicy::Required,
};

#[axum::async_trait]
impl CompiledRouteInvocation for CompiledSessionEventsInvocation {
    fn contract(&self) -> &'static RouteInvocationContract {
        &SESSION_EVENTS_CONTRACT
    }

    async fn invoke(
        &self,
        ctx: RouteInvocationContext,
    ) -> Result<RouteInvocationResult, RouteDispatchError> {
        let session_id = ctx
            .input
            .get("session_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                RouteDispatchError::BadRequest("missing session_id in source_config".into())
            })?;

        let session = ctx
            .state
            .browser_sessions
            .get_session(session_id)
            .ok_or(RouteDispatchError::Unauthorized)?;

        // Verify the principal matches the session.
        if let Some(ref principal) = ctx.principal {
            let expected = format!("session:{}", session.session_id);
            if principal.id != expected {
                return Err(RouteDispatchError::Forbidden(format!(
                    "session mismatch: principal {} != session {}",
                    principal.id, expected
                )));
            }
        }

        // Replay from Last-Event-ID if present.
        let replay = ctx
            .headers
            .get("Last-Event-ID")
            .and_then(|v| v.to_str().ok())
            .and_then(|last_id| {
                ctx.state.session_bus.replay_after(session_id, last_id)
            });

        let mut receiver = ctx.state.session_bus.subscribe(session_id);
        let keep_alive_secs = self.keep_alive_secs;

        let stream: std::pin::Pin<
            Box<
                dyn Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>
                    + Send,
            >,
        > = if let Some(events) = replay {
            if events.is_empty() {
                let snap = ctx.state.session_bus.snapshot_required_envelope();
                let snap_event = envelope_to_sse(&snap);
                Box::pin(async_stream::stream! {
                    yield Ok(snap_event);
                    loop {
                        match receiver.recv().await {
                            Ok(envelope) => yield Ok(envelope_to_sse(&envelope)),
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                                yield Ok(envelope_to_sse(&ctx.state.session_bus.snapshot_required_envelope()));
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        }
                    }
                })
            } else {
                Box::pin(async_stream::stream! {
                    for e in events {
                        yield Ok(envelope_to_sse(&e));
                    }
                    loop {
                        match receiver.recv().await {
                            Ok(envelope) => yield Ok(envelope_to_sse(&envelope)),
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                                yield Ok(envelope_to_sse(&ctx.state.session_bus.snapshot_required_envelope()));
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        }
                    }
                })
            }
        } else {
            Box::pin(async_stream::stream! {
                loop {
                    match receiver.recv().await {
                        Ok(envelope) => yield Ok(envelope_to_sse(&envelope)),
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            yield Ok(envelope_to_sse(&ctx.state.session_bus.snapshot_required_envelope()));
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            })
        };

        Ok(RouteInvocationResult::Stream(RouteEventStream {
            events: stream,
            keep_alive_secs,
        }))
    }
}

/// Convert a `RouteStreamEnvelope` to an SSE `Event`.
fn envelope_to_sse(envelope: &RouteStreamEnvelope) -> axum::response::sse::Event {
    use axum::response::sse::Event;
    let data = serde_json::to_string(&envelope.payload).unwrap_or_default();
    let mut event = Event::default()
        .event(envelope.event_type.clone())
        .data(data);
    if let Some(ref id) = envelope.id {
        event = event.id(id.clone());
    }
    event
}
