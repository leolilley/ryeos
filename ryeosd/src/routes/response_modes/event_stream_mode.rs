use std::sync::Arc;
use std::time::Duration;

use axum::response::sse::{KeepAlive, Sse};
use axum::response::IntoResponse;

use crate::dispatch_error::{RouteConfigError, RouteDispatchError};
use crate::routes::compile::{
    CompiledResponseMode, CompiledRoute, ModeCompileContext, ResponseMode, RouteDispatchContext,
};
use crate::routes::raw::RawRouteSpec;
use crate::routes::streaming_sources::{
    BoundStreamingSource, RawEventStreamResponse,
};

pub struct EventStreamMode;

pub struct CompiledEventStreamMode {
    bound_source: Arc<dyn BoundStreamingSource>,
}

impl ResponseMode for EventStreamMode {
    fn key(&self) -> &'static str {
        "event_stream"
    }

    fn allows_zero_timeout(&self) -> bool {
        true
    }

    fn compile(
        &self,
        raw: &RawRouteSpec,
        ctx: &ModeCompileContext,
    ) -> Result<Arc<dyn CompiledResponseMode>, RouteConfigError> {
        if raw.execute.is_some() {
            return Err(RouteConfigError::InvalidResponseSpec {
                id: raw.id.clone(),
                mode: "event_stream".into(),
                reason: "event_stream mode must not have a top-level 'execute' block".into(),
            });
        }

        let source_key = raw.response.source.as_deref()
            .ok_or_else(|| RouteConfigError::InvalidResponseSpec {
                id: raw.id.clone(),
                mode: "event_stream".into(),
                reason: "event_stream mode requires `response.source` (e.g. `thread_events`)".into(),
            })?;

        let streaming_source = ctx
            .streaming_sources
            .get(source_key)
            .ok_or_else(|| RouteConfigError::UnknownStreamingSource {
                id: raw.id.clone(),
                src: source_key.into(),
            })?;

        let raw_event_stream = RawEventStreamResponse {
            source: source_key.into(),
            source_config: raw.response.source_config.clone(),
        };

        let source_ctx = crate::routes::streaming_sources::SourceCompileContext {
            auth_verifier_key: &raw.auth,
        };

        let bound = streaming_source.compile(raw, &raw_event_stream, &source_ctx)?;

        Ok(Arc::new(CompiledEventStreamMode {
            bound_source: bound,
        }))
    }
}

#[axum::async_trait]
impl CompiledResponseMode for CompiledEventStreamMode {
    fn is_streaming(&self) -> bool {
        true
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn handle(
        &self,
        _compiled: &CompiledRoute,
        ctx: RouteDispatchContext,
    ) -> Result<axum::response::Response, RouteDispatchError> {
        let last_event_id = parse_last_event_id(&ctx.request_parts.headers)?;

        let sse_result = self
            .bound_source
            .open(&ctx, last_event_id, &ctx.state)
            .await?;

        let body_stream = sse_result.stream;
        let keep_alive_secs = sse_result.keep_alive_secs.max(1);

        let keep_alive = KeepAlive::new()
            .interval(Duration::from_secs(keep_alive_secs))
            .text(":");

        let sse = Sse::new(body_stream).keep_alive(keep_alive);

        Ok(sse.into_response())
    }
}

fn parse_last_event_id(
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::streaming_sources::StreamingSourceRegistry;

    #[test]
    fn parse_valid_last_event_id() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("last-event-id", "42".parse().unwrap());
        assert_eq!(parse_last_event_id(&headers).unwrap(), Some(42));
    }

    #[test]
    fn parse_missing_last_event_id() {
        let headers = axum::http::HeaderMap::new();
        assert_eq!(parse_last_event_id(&headers).unwrap(), None);
    }

    #[test]
    fn parse_non_numeric_last_event_id_rejected() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("last-event-id", "abc".parse().unwrap());
        assert!(matches!(
            parse_last_event_id(&headers),
            Err(RouteDispatchError::BadLastEventId)
        ));
    }

    #[test]
    fn parse_empty_last_event_id_rejected() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("last-event-id", "".parse().unwrap());
        assert!(matches!(
            parse_last_event_id(&headers),
            Err(RouteDispatchError::BadLastEventId)
        ));
    }

    #[test]
    fn parse_negative_last_event_id_rejected() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("last-event-id", "-1".parse().unwrap());
        assert!(matches!(
            parse_last_event_id(&headers),
            Err(RouteDispatchError::BadLastEventId)
        ));
    }

    #[test]
    fn allows_zero_timeout() {
        assert!(EventStreamMode.allows_zero_timeout());
    }

    fn make_raw_with_execute() -> RawRouteSpec {
        use crate::routes::raw::{RawExecute, RawLimits, RawRequest, RawRequestBody, RawResponseSpec};
        RawRouteSpec {
            section: "routes".into(),
            id: "test-route".into(),
            path: "/test".into(),
            methods: ["GET".into()].into_iter().collect(),
            auth: "none".into(),
            auth_config: None,
            limits: RawLimits::default(),
            response: RawResponseSpec {
                mode: "event_stream".into(),
                source: Some("thread_events".into()),
                source_config: serde_json::json!({
                    "thread_id": "${path.id}",
                }),
                status: None,
                content_type: None,
                body_b64: None,
            },
            execute: Some(RawExecute {
                item_ref: "tool:x/y".into(),
                params: serde_json::Value::Null,
            }),
            request: RawRequest {
                body: RawRequestBody::None,
            },
            source_file: std::path::PathBuf::from("/test/r.yaml"),
        }
    }

    #[test]
    fn compile_rejects_execute_block() {
        let mode = EventStreamMode;
        let raw = make_raw_with_execute();
        let ctx = ModeCompileContext {
            streaming_sources: &StreamingSourceRegistry::with_builtins(),
        };
        let result = mode.compile(&raw, &ctx);
        let err = match result {
            Err(e) => e,
            Ok(_) => panic!("expected error"),
        };
        let msg = format!("{err}");
        assert!(
            msg.contains("must not have a top-level 'execute' block"),
            "got: {msg}"
        );
    }

    #[test]
    fn compile_rejects_unknown_source() {
        use crate::routes::raw::{RawLimits, RawRequest, RawRequestBody, RawResponseSpec};
        let mode = EventStreamMode;
        let raw = RawRouteSpec {
            section: "routes".into(),
            id: "test-route".into(),
            path: "/test/{id}".into(),
            methods: ["GET".into()].into_iter().collect(),
            auth: "none".into(),
            auth_config: None,
            limits: RawLimits::default(),
            response: RawResponseSpec {
                mode: "event_stream".into(),
                source: Some("nonexistent_source".into()),
                source_config: serde_json::Value::Null,
                status: None,
                content_type: None,
                body_b64: None,
            },
            execute: None,
            request: RawRequest {
                body: RawRequestBody::None,
            },
            source_file: std::path::PathBuf::from("/test/r.yaml"),
        };
        let ctx = ModeCompileContext {
            streaming_sources: &StreamingSourceRegistry::with_builtins(),
        };
        let result = mode.compile(&raw, &ctx);
        let err = match result {
            Err(e) => e,
            Ok(_) => panic!("expected error"),
        };
        let msg = format!("{err}");
        assert!(
            msg.contains("unknown streaming source"),
            "got: {msg}"
        );
    }

    #[test]
    fn compile_rejects_missing_source() {
        use crate::routes::raw::{RawLimits, RawRequest, RawRequestBody, RawResponseSpec};
        let mode = EventStreamMode;
        let raw = RawRouteSpec {
            section: "routes".into(),
            id: "test-route".into(),
            path: "/test/{id}".into(),
            methods: ["GET".into()].into_iter().collect(),
            auth: "none".into(),
            auth_config: None,
            limits: RawLimits::default(),
            response: RawResponseSpec {
                mode: "event_stream".into(),
                source: None,
                source_config: serde_json::Value::Null,
                status: None,
                content_type: None,
                body_b64: None,
            },
            execute: None,
            request: RawRequest {
                body: RawRequestBody::None,
            },
            source_file: std::path::PathBuf::from("/test/r.yaml"),
        };
        let ctx = ModeCompileContext {
            streaming_sources: &StreamingSourceRegistry::with_builtins(),
        };
        let result = mode.compile(&raw, &ctx);
        let err = match result {
            Err(e) => e,
            Ok(_) => panic!("expected error"),
        };
        let msg = format!("{err}");
        assert!(
            msg.contains("requires `response.source`"),
            "got: {msg}"
        );
    }
}
