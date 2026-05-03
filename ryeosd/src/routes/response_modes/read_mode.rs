//! Unary `read` response mode — returns JSON from a `ReadSource`.
//!
//! Wire shape: HTTP request → verify auth → look up bound read source →
//! call `fetch(captures, state)` → return `200 JSON` or `404`.
//!
//! Mirrors `event_stream_mode.rs` but for synchronous JSON responses
//! instead of SSE streams. The read source plugin system mirrors
//! `StreamingSource` / `StreamingSourceRegistry`.

use std::sync::Arc;

use axum::http::StatusCode;
use axum::response::IntoResponse;

use crate::dispatch_error::{RouteConfigError, RouteDispatchError};
use crate::routes::compile::{
    CompiledResponseMode, CompiledRoute, ModeCompileContext, ResponseMode, RouteDispatchContext,
};
use crate::routes::raw::RawRouteSpec;

pub struct ReadMode;

pub struct CompiledReadMode {
    bound_source: Arc<dyn crate::routes::read_sources::BoundReadSource>,
}

impl ResponseMode for ReadMode {
    fn key(&self) -> &'static str {
        "read"
    }

    fn compile(
        &self,
        raw: &RawRouteSpec,
        ctx: &ModeCompileContext,
    ) -> Result<Arc<dyn CompiledResponseMode>, RouteConfigError> {
        if raw.execute.is_some() {
            return Err(RouteConfigError::InvalidResponseSpec {
                id: raw.id.clone(),
                mode: "read".into(),
                reason: "read mode must not have a top-level 'execute' block".into(),
            });
        }

        // Reject static-mode fields
        if raw.response.status.is_some()
            || raw.response.content_type.is_some()
            || raw.response.body_b64.is_some()
        {
            return Err(RouteConfigError::InvalidResponseSpec {
                id: raw.id.clone(),
                mode: "read".into(),
                reason: "read mode must not set static-mode fields \
                    (status / content_type / body_b64)"
                    .into(),
            });
        }

        let source_key = raw.response.source.as_deref().ok_or_else(|| {
            RouteConfigError::InvalidResponseSpec {
                id: raw.id.clone(),
                mode: "read".into(),
                reason: "read mode requires `response.source` (e.g. `thread_detail`)".into(),
            }
        })?;

        let read_source = ctx
            .read_sources
            .get(source_key)
            .ok_or_else(|| RouteConfigError::UnknownReadSource {
                id: raw.id.clone(),
                src: source_key.into(),
            })?;

        let bound = read_source.compile(raw, &raw.response.source_config)?;

        Ok(Arc::new(CompiledReadMode {
            bound_source: bound,
        }))
    }
}

#[axum::async_trait]
impl CompiledResponseMode for CompiledReadMode {
    fn is_streaming(&self) -> bool {
        false
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn handle(
        &self,
        _compiled: &CompiledRoute,
        ctx: RouteDispatchContext,
    ) -> Result<axum::response::Response, RouteDispatchError> {
        match self
            .bound_source
            .fetch(&ctx.captures, &ctx.state)
            .await?
        {
            Some(value) => Ok(axum::Json(value).into_response()),
            None => Ok((
                StatusCode::NOT_FOUND,
                axum::Json(serde_json::json!({ "error": "not found" })),
            )
                .into_response()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::read_sources::ReadSourceRegistry;

    fn compile_ctx() -> ModeCompileContext<'static> {
        use std::sync::OnceLock;
        static STREAMING: OnceLock<crate::routes::streaming_sources::StreamingSourceRegistry> = OnceLock::new();
        static READ: OnceLock<ReadSourceRegistry> = OnceLock::new();
        ModeCompileContext {
            streaming_sources: STREAMING.get_or_init(crate::routes::streaming_sources::StreamingSourceRegistry::new),
            read_sources: READ.get_or_init(ReadSourceRegistry::with_builtins),
        }
    }

    fn make_raw(path: &str, source: Option<&str>, source_config: serde_json::Value) -> RawRouteSpec {
        use crate::routes::raw::{RawLimits, RawRequest, RawRequestBody, RawResponseSpec};
        RawRouteSpec {
            section: "routes".into(),
            id: "test-route".into(),
            path: path.into(),
            methods: ["GET".into()].into_iter().collect(),
            auth: "rye_signed".into(),
            auth_config: None,
            limits: RawLimits::default(),
            response: RawResponseSpec {
                mode: "read".into(),
                source: source.map(|s| s.into()),
                source_config,
                status: None,
                content_type: None,
                body_b64: None,
            },
            execute: None,
            request: RawRequest {
                body: RawRequestBody::None,
            },
            source_file: std::path::PathBuf::from("/test/r.yaml"),
        }
    }

    #[test]
    fn compile_valid_thread_detail() {
        let mode = ReadMode;
        let raw = make_raw(
            "/threads/{thread_id}",
            Some("thread_detail"),
            serde_json::json!({ "thread_id": "${path.thread_id}" }),
        );
        let result = mode.compile(&raw, &compile_ctx());
        assert!(result.is_ok(), "expected Ok, got: {:?}", result.err());
    }

    #[test]
    fn compile_rejects_missing_source() {
        let mode = ReadMode;
        let raw = make_raw("/test", None, serde_json::Value::Null);
        let result = mode.compile(&raw, &compile_ctx());
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

    #[test]
    fn compile_rejects_unknown_source() {
        let mode = ReadMode;
        let raw = make_raw(
            "/test",
            Some("nonexistent_source"),
            serde_json::Value::Null,
        );
        let result = mode.compile(&raw, &compile_ctx());
        let err = match result {
            Err(e) => e,
            Ok(_) => panic!("expected error"),
        };
        let msg = format!("{err}");
        assert!(msg.contains("unknown read source"), "got: {msg}");
    }

    #[test]
    fn compile_rejects_static_fields() {
        use crate::routes::raw::{RawLimits, RawRequest, RawRequestBody, RawResponseSpec};
        let mode = ReadMode;
        let raw = RawRouteSpec {
            section: "routes".into(),
            id: "test-route".into(),
            path: "/test".into(),
            methods: ["GET".into()].into_iter().collect(),
            auth: "rye_signed".into(),
            auth_config: None,
            limits: RawLimits::default(),
            response: RawResponseSpec {
                mode: "read".into(),
                source: Some("thread_detail".into()),
                source_config: serde_json::json!({ "thread_id": "${path.id}" }),
                status: Some(200),
                content_type: None,
                body_b64: None,
            },
            execute: None,
            request: RawRequest {
                body: RawRequestBody::None,
            },
            source_file: std::path::PathBuf::from("/test/r.yaml"),
        };
        let result = mode.compile(&raw, &compile_ctx());
        let err = match result {
            Err(e) => e,
            Ok(_) => panic!("expected error"),
        };
        let msg = format!("{err}");
        assert!(
            msg.contains("must not set static-mode fields"),
            "got: {msg}"
        );
    }

    #[test]
    fn compile_rejects_execute_block() {
        use crate::routes::raw::{RawExecute, RawLimits, RawRequest, RawRequestBody, RawResponseSpec};
        let mode = ReadMode;
        let raw = RawRouteSpec {
            section: "routes".into(),
            id: "test-route".into(),
            path: "/test".into(),
            methods: ["GET".into()].into_iter().collect(),
            auth: "rye_signed".into(),
            auth_config: None,
            limits: RawLimits::default(),
            response: RawResponseSpec {
                mode: "read".into(),
                source: Some("thread_detail".into()),
                source_config: serde_json::json!({ "thread_id": "${path.id}" }),
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
        };
        let result = mode.compile(&raw, &compile_ctx());
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
}
