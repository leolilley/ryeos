//! `session_events` invoker — subscribes to a session's event bus.
//!
//! Reads from `SessionBus::subscribe(session_id)` and yields envelopes.
//! The SSE transport layer in `event_stream_mode` converts them to wire frames.
//! The browser's `/ui/events/session/{id}` route uses this invoker.

use std::sync::Arc;

use ryeos_api::route_error::{RouteConfigError, RouteDispatchError};
use ryeos_api::routes::invocation::{
    CompiledRouteInvocation, PrincipalPolicy, RouteEventStream, RouteInvocationContext,
    RouteInvocationContract, RouteInvocationOutput, RouteInvocationResult,
};
use ryeos_api::routes::response_modes::event_stream_mode::{
    validate_and_extract_path_capture, EventStreamStrategy, StreamSourceCompiler,
};
use ryeos_app::route_raw::RawRouteSpec;
use tokio_stream::Stream;

use crate::state::UiState;

pub struct CompiledSessionEventsInvocation {
    pub ui: Arc<UiState>,
    pub keep_alive_secs: u64,
}

/// Stream source compiler for `session_events`.
///
/// Registered as `"session_events"` in the event stream source registry by
/// the UI composition root. UI owns the source name, auth requirement, config
/// validation, and invoker construction; the API mode only frames the stream.
pub struct SessionEventsSourceFactory {
    pub ui: Arc<UiState>,
}

impl StreamSourceCompiler for SessionEventsSourceFactory {
    fn compile(
        &self,
        raw: &RawRouteSpec,
    ) -> Result<EventStreamStrategy, ryeos_api::route_error::RouteConfigError> {
        let cfg = compile_session_events_config(raw)?;
        let invoker: Arc<dyn CompiledRouteInvocation> = Arc::new(CompiledSessionEventsInvocation {
            ui: self.ui.clone(),
            keep_alive_secs: cfg.keep_alive_secs,
        });
        Ok(EventStreamStrategy::PathCaptureInput {
            invoker,
            input_field: "session_id".into(),
            capture_name: cfg.session_id_capture,
        })
    }
}

const SESSION_EVENTS_SOURCE: &str = "session_events";
const SESSION_EVENTS_REQUIRED_AUTH: &str = "browser_session";

struct SessionEventsConfig {
    session_id_capture: String,
    keep_alive_secs: u64,
}

fn compile_session_events_config(
    raw: &RawRouteSpec,
) -> Result<SessionEventsConfig, RouteConfigError> {
    if raw.auth != SESSION_EVENTS_REQUIRED_AUTH {
        return Err(RouteConfigError::SourceAuthRequirement {
            id: raw.id.clone(),
            src: SESSION_EVENTS_SOURCE.into(),
            required: SESSION_EVENTS_REQUIRED_AUTH.into(),
            got: raw.auth.clone(),
        });
    }

    let source_config = &raw.response.source_config;
    let session_id_template = source_config
        .get("session_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| RouteConfigError::InvalidSourceConfig {
            id: raw.id.clone(),
            src: SESSION_EVENTS_SOURCE.into(),
            reason: "missing 'session_id' in source_config".into(),
        })?;

    let session_id_capture = validate_and_extract_path_capture(
        session_id_template,
        SESSION_EVENTS_SOURCE,
        "session_id",
        &raw.id,
        &raw.path,
    )?;

    let keep_alive_secs = source_config
        .get("keep_alive_secs")
        .and_then(|v| v.as_u64())
        .unwrap_or(15);
    if keep_alive_secs == 0 {
        return Err(RouteConfigError::InvalidSourceConfig {
            id: raw.id.clone(),
            src: SESSION_EVENTS_SOURCE.into(),
            reason: "keep_alive_secs must be > 0".into(),
        });
    }

    Ok(SessionEventsConfig {
        session_id_capture,
        keep_alive_secs,
    })
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

        let session = self
            .ui
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
            .and_then(|last_id| self.ui.session_bus.replay_after(session_id, last_id));

        let mut receiver = self.ui.session_bus.subscribe(session_id);
        let keep_alive_secs = self.keep_alive_secs;

        let stream: std::pin::Pin<
            Box<
                dyn Stream<
                        Item = Result<
                            ryeos_app::stream_envelope::RouteStreamEnvelope,
                            std::convert::Infallible,
                        >,
                    > + Send,
            >,
        > = if let Some(events) = replay {
            if events.is_empty() {
                let snap = crate::session_bus::SessionBus::snapshot_required_envelope();
                Box::pin(async_stream::stream! {
                    yield Ok(snap);
                    loop {
                        match receiver.recv().await {
                            Ok(envelope) => yield Ok(envelope),
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                                yield Ok(crate::session_bus::SessionBus::snapshot_required_envelope());
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        }
                    }
                })
            } else {
                Box::pin(async_stream::stream! {
                    for e in events {
                        yield Ok(e);
                    }
                    loop {
                        match receiver.recv().await {
                            Ok(envelope) => yield Ok(envelope),
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                                yield Ok(crate::session_bus::SessionBus::snapshot_required_envelope());
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
                        Ok(envelope) => yield Ok(envelope),
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            yield Ok(crate::session_bus::SessionBus::snapshot_required_envelope());
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
