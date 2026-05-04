use std::sync::Arc;

use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::dispatch_error::{RouteConfigError, RouteDispatchError};
use crate::routes::compile::{
    CompiledResponseMode, CompiledRoute, ModeCompileContext, ResponseMode, RouteDispatchContext,
};
use crate::routes::raw::RawRouteSpec;

pub struct StaticMode;

pub struct CompiledStaticMode {
    status: StatusCode,
    content_type: HeaderValue,
    body: Vec<u8>,
}

impl ResponseMode for StaticMode {
    fn key(&self) -> &'static str {
        "static"
    }

    fn compile(
        &self,
        raw: &RawRouteSpec,
        _ctx: &ModeCompileContext,
    ) -> Result<Arc<dyn CompiledResponseMode>, RouteConfigError> {
        let status_raw = raw.response.status.ok_or_else(|| RouteConfigError::InvalidResponseSpec {
            id: raw.id.clone(),
            mode: "static".into(),
            reason: "missing 'status' field".into(),
        })?;

        let content_type = raw.response.content_type.as_ref().ok_or_else(|| {
            RouteConfigError::InvalidResponseSpec {
                id: raw.id.clone(),
                mode: "static".into(),
                reason: "missing 'content_type' field".into(),
            }
        })?;

        let body_b64 = raw.response.body_b64.as_ref().ok_or_else(|| {
            RouteConfigError::InvalidResponseSpec {
                id: raw.id.clone(),
                mode: "static".into(),
                reason: "missing 'body_b64' field".into(),
            }
        })?;

        let status = StatusCode::from_u16(status_raw).map_err(|_| RouteConfigError::InvalidResponseSpec {
            id: raw.id.clone(),
            mode: "static".into(),
            reason: format!("invalid HTTP status code: {status_raw}"),
        })?;

        let header_val = content_type.parse::<HeaderValue>().map_err(|_| {
            RouteConfigError::InvalidResponseSpec {
                id: raw.id.clone(),
                mode: "static".into(),
                reason: format!("invalid content_type header value: {content_type}"),
            }
        })?;

        use base64::Engine;
        let body = base64::engine::general_purpose::STANDARD
            .decode(body_b64)
            .map_err(|e| RouteConfigError::InvalidResponseSpec {
                id: raw.id.clone(),
                mode: "static".into(),
                reason: format!("body_b64 is not valid base64: {e}"),
            })?;

        Ok(Arc::new(CompiledStaticMode {
            status,
            content_type: header_val,
            body,
        }))
    }
}

#[axum::async_trait]
impl CompiledResponseMode for CompiledStaticMode {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn handle(
        &self,
        _compiled: &CompiledRoute,
        _ctx: RouteDispatchContext,
    ) -> Result<Response, RouteDispatchError> {
        let mut resp = (self.status, self.body.clone()).into_response();
        resp.headers_mut()
            .insert(axum::http::header::CONTENT_TYPE, self.content_type.clone());
        Ok(resp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::raw::{RawLimits, RawRequest, RawRequestBody, RawResponseSpec, RawRouteSpec};

    fn make_raw(status: Option<u16>, content_type: Option<&str>, body_b64: Option<&str>) -> RawRouteSpec {
        RawRouteSpec {
            section: "routes".to_string(),
            id: "test-route".to_string(),
            path: "/test".to_string(),
            methods: ["GET".to_string()].into_iter().collect(),
            auth: "none".to_string(),
            auth_config: None,
            limits: RawLimits::default(),
            response: RawResponseSpec {
                mode: "static".to_string(),
                source: None,
                source_config: serde_json::Value::Null,
                status,
                content_type: content_type.map(|s| s.to_string()),
                body_b64: body_b64.map(|s| s.to_string()),
            },
            execute: None,
            request: RawRequest {
                body: RawRequestBody::None,
            },
            source_file: std::path::PathBuf::from("/test/r.yaml"),
        }
    }

    fn compile_ctx() -> ModeCompileContext<'static> {
        ModeCompileContext {
            _phantom: std::marker::PhantomData,
        }
    }

    #[test]
    fn valid_static_compiles() {
        let mode = StaticMode;
        let raw = make_raw(Some(200), Some("text/plain"), Some("aGVsbG8="));
        let result = mode.compile(&raw, &compile_ctx());
        assert!(result.is_ok());
        let compiled = match result {
            Ok(c) => c,
            Err(e) => panic!("expected Ok, got: {e}"),
        };
        let inner = compiled
            .as_ref()
            .as_any()
            .downcast_ref::<CompiledStaticMode>()
            .expect("downcast to CompiledStaticMode");
        assert_eq!(inner.status, StatusCode::OK);
        assert_eq!(inner.body, b"hello");
    }

    #[test]
    fn bad_base64_rejected() {
        let mode = StaticMode;
        let raw = make_raw(Some(200), Some("text/plain"), Some("not-valid-base64!!!"));
        let result = mode.compile(&raw, &compile_ctx());
        match result {
            Ok(_) => panic!("expected error"),
            Err(e) => {
                let msg = format!("{e}");
                assert!(msg.contains("base64"), "got: {msg}");
            }
        }
    }

    #[test]
    fn bad_status_code_rejected() {
        let mode = StaticMode;
        let raw = make_raw(Some(99), Some("text/plain"), Some("aGVsbG8="));
        let result = mode.compile(&raw, &compile_ctx());
        match result {
            Ok(_) => panic!("expected error"),
            Err(e) => {
                let msg = format!("{e}");
                assert!(msg.contains("invalid HTTP status code"), "got: {msg}");
            }
        }
    }

    #[test]
    fn missing_status_rejected() {
        let mode = StaticMode;
        let raw = make_raw(None, Some("text/plain"), Some("aGVsbG8="));
        let result = mode.compile(&raw, &compile_ctx());
        match result {
            Ok(_) => panic!("expected error"),
            Err(e) => {
                let msg = format!("{e}");
                assert!(msg.contains("missing 'status'"), "got: {msg}");
            }
        }
    }

    #[test]
    fn missing_content_type_rejected() {
        let mode = StaticMode;
        let raw = make_raw(Some(200), None, Some("aGVsbG8="));
        let result = mode.compile(&raw, &compile_ctx());
        match result {
            Ok(_) => panic!("expected error"),
            Err(e) => {
                let msg = format!("{e}");
                assert!(msg.contains("missing 'content_type'"), "got: {msg}");
            }
        }
    }

    #[test]
    fn missing_body_b64_rejected() {
        let mode = StaticMode;
        let raw = make_raw(Some(200), Some("text/plain"), None);
        let result = mode.compile(&raw, &compile_ctx());
        match result {
            Ok(_) => panic!("expected error"),
            Err(e) => {
                let msg = format!("{e}");
                assert!(msg.contains("missing 'body_b64'"), "got: {msg}");
            }
        }
    }
}
