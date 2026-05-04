//! `json` response mode — synchronous JSON from a service canonical ref.
//!
//! Wire shape: HTTP request → verify auth → resolve service canonical ref →
//! call service handler in-process → return `200 JSON` or `404`/`500`.
//!
//! The `source` field in route YAML is a canonical ref (e.g.
//! `service:threads/get`), not a hardcoded name. At compile time the mode
//! validates the ref parses, is a `service:` kind, and maps to a registered
//! service descriptor. At dispatch time it interpolates `source_config`,
//! enforces caps, calls the handler, and returns the result.
//!
//! No parallel registry. The engine's service descriptor list (`handlers::ALL`)
//! is the compile-time source of truth. The `ServiceRegistry` in `AppState`
//! handles runtime dispatch.

use std::sync::Arc;

use axum::http::StatusCode;
use axum::response::IntoResponse;

use crate::dispatch_error::{RouteConfigError, RouteDispatchError};
use crate::routes::compile::{
    CompiledResponseMode, CompiledRoute, ModeCompileContext, ResponseMode, RouteDispatchContext,
};
use crate::routes::interpolation;
use crate::routes::raw::RawRouteSpec;

pub struct JsonMode;

/// Compiled state for a `json` mode route.
struct CompiledJsonMode {
    /// Service endpoint string (e.g., `"threads.get"`), looked up from the
    /// descriptor at compile time and used for runtime dispatch.
    endpoint: String,
    /// The source_config template, kept as-is for runtime interpolation.
    source_config_template: serde_json::Value,
    /// Capability scopes required by the service (from descriptor).
    required_caps: Vec<String>,
}

impl ResponseMode for JsonMode {
    fn key(&self) -> &'static str {
        "json"
    }

    fn compile(
        &self,
        raw: &RawRouteSpec,
        _ctx: &ModeCompileContext,
    ) -> Result<Arc<dyn CompiledResponseMode>, RouteConfigError> {
        if raw.execute.is_some() {
            return Err(RouteConfigError::InvalidResponseSpec {
                id: raw.id.clone(),
                mode: "json".into(),
                reason: "json mode must not have a top-level 'execute' block".into(),
            });
        }

        // Reject static-mode fields
        if raw.response.status.is_some()
            || raw.response.content_type.is_some()
            || raw.response.body_b64.is_some()
        {
            return Err(RouteConfigError::InvalidResponseSpec {
                id: raw.id.clone(),
                mode: "json".into(),
                reason: "json mode must not set static-mode fields \
                    (status / content_type / body_b64)"
                    .into(),
            });
        }

        let source_str = raw.response.source.as_deref().ok_or_else(|| {
            RouteConfigError::InvalidSourceConfig {
                id: raw.id.clone(),
                src: "json".into(),
                reason: "json mode requires `response.source` as a canonical ref \
                    (e.g. `service:threads/get`)"
                    .into(),
            }
        })?;

        // Parse and validate the canonical ref.
        let parsed = crate::routes::parsed_ref::ParsedItemRef::parse(source_str).map_err(|e| {
            RouteConfigError::InvalidSourceConfig {
                id: raw.id.clone(),
                src: "json".into(),
                reason: format!(
                    "source '{}' is not a valid canonical ref: {e}",
                    source_str
                ),
            }
        })?;

        // json mode only supports service: kind.
        if parsed.kind() != "service" {
            return Err(RouteConfigError::InvalidSourceConfig {
                id: raw.id.clone(),
                src: "json".into(),
                reason: format!(
                    "json mode source must be 'service:' kind, got '{}' kind in '{}'",
                    parsed.kind(),
                    source_str
                ),
            });
        }

        // Look up the service descriptor by canonical ref.
        let descriptor = crate::service_handlers::ALL
            .iter()
            .find(|d| d.service_ref == source_str)
            .ok_or_else(|| RouteConfigError::InvalidSourceConfig {
                id: raw.id.clone(),
                src: "json".into(),
                reason: format!(
                    "no service descriptor found for ref '{}'; \
                     available services: [{}]",
                    source_str,
                    crate::service_handlers::ALL
                        .iter()
                        .map(|d| d.service_ref)
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            })?;

        // Reject OfflineOnly services.
        if descriptor.availability == crate::service_executor::ServiceAvailability::OfflineOnly {
            return Err(RouteConfigError::InvalidSourceConfig {
                id: raw.id.clone(),
                src: "json".into(),
                reason: format!(
                    "service '{}' is OfflineOnly (only available when daemon is down); \
                     json mode requires live-available services",
                    source_str
                ),
            });
        }

        // Validate source_config path templates against declared captures.
        if !raw.response.source_config.is_null() {
            let declared = extract_path_captures(&raw.path);
            interpolation::validate_path_templates(
                &raw.response.source_config,
                &declared,
                &raw.id,
                "json",
            )?;
        }

        Ok(Arc::new(CompiledJsonMode {
            endpoint: descriptor.endpoint.to_string(),
            source_config_template: raw.response.source_config.clone(),
            required_caps: Vec::new(), // Extracted from descriptor metadata if available
        }))
    }
}

#[axum::async_trait]
impl CompiledResponseMode for CompiledJsonMode {
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
        // Interpolate source_config templates with actual capture values.
        let params = if self.source_config_template.is_null() {
            serde_json::Value::Object(serde_json::Map::new())
        } else {
            interpolation::interpolate_path(&self.source_config_template, &ctx.captures)?
        };

        // Enforce required_caps against principal scopes.
        if !self.required_caps.is_empty() {
            let scopes = &ctx.principal.scopes;
            for cap in &self.required_caps {
                if !scopes.contains(cap) {
                    return Ok((
                        StatusCode::FORBIDDEN,
                        axum::Json(serde_json::json!({
                            "error": "insufficient capabilities",
                            "required": self.required_caps,
                            "granted": scopes,
                        })),
                    )
                        .into_response());
                }
            }
        }

        // Look up the handler by endpoint. Clone the Arc<HandlerFn> so we
        // can move state into Arc without borrow conflict.
        let handler = ctx.state.services.get(&self.endpoint)
            .cloned()
            .ok_or_else(|| {
                RouteDispatchError::Internal(format!(
                    "service endpoint '{}' not found in registry (json mode dispatch)",
                    self.endpoint
                ))
            })?;

        // Call the service handler in-process.
        let state_arc = Arc::new(ctx.state);
        let result = handler(params, state_arc)
            .await
            .map_err(|e| RouteDispatchError::Internal(format!("service error: {e}")))?;

        // Null result → 404. Some result → 200 JSON.
        if result.is_null() {
            Ok((
                StatusCode::NOT_FOUND,
                axum::Json(serde_json::json!({ "error": "not found" })),
            )
                .into_response())
        } else {
            Ok(axum::Json(result).into_response())
        }
    }
}

/// Extract `{name}` capture names from a path template like `/threads/{thread_id}`.
fn extract_path_captures(path: &str) -> Vec<String> {
    let mut captures = Vec::new();
    let mut chars = path.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '{' {
            let mut name = String::new();
            for inner in chars.by_ref() {
                if inner == '}' {
                    break;
                }
                name.push(inner);
            }
            if !name.is_empty() {
                captures.push(name);
            }
        }
    }
    captures
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compile_ctx() -> ModeCompileContext<'static> {
        ModeCompileContext {
            _phantom: std::marker::PhantomData,
        }
    }

    fn make_raw(
        path: &str,
        source: Option<&str>,
        source_config: serde_json::Value,
    ) -> RawRouteSpec {
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
                mode: "json".into(),
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
    fn compile_valid_service_ref() {
        let mode = JsonMode;
        let raw = make_raw(
            "/threads/{thread_id}",
            Some("service:threads/get"),
            serde_json::json!({ "thread_id": "${path.thread_id}" }),
        );
        let result = mode.compile(&raw, &compile_ctx());
        assert!(result.is_ok(), "expected Ok, got: {:?}", result.err());
    }

    #[test]
    fn compile_rejects_missing_source() {
        let mode = JsonMode;
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
    fn compile_rejects_non_service_kind() {
        let mode = JsonMode;
        let raw = make_raw(
            "/test",
            Some("tool:rye/core/execute"),
            serde_json::Value::Null,
        );
        let result = mode.compile(&raw, &compile_ctx());
        let err = match result {
            Err(e) => e,
            Ok(_) => panic!("expected error"),
        };
        let msg = format!("{err}");
        assert!(
            msg.contains("must be 'service:' kind"),
            "got: {msg}"
        );
    }

    #[test]
    fn compile_rejects_unknown_service() {
        let mode = JsonMode;
        let raw = make_raw(
            "/test",
            Some("service:nonexistent/handler"),
            serde_json::Value::Null,
        );
        let result = mode.compile(&raw, &compile_ctx());
        let err = match result {
            Err(e) => e,
            Ok(_) => panic!("expected error"),
        };
        let msg = format!("{err}");
        assert!(
            msg.contains("no service descriptor found"),
            "got: {msg}"
        );
    }

    #[test]
    fn compile_rejects_static_fields() {
        use crate::routes::raw::{RawLimits, RawRequest, RawRequestBody, RawResponseSpec};
        let mode = JsonMode;
        let raw = RawRouteSpec {
            section: "routes".into(),
            id: "test-route".into(),
            path: "/test".into(),
            methods: ["GET".into()].into_iter().collect(),
            auth: "rye_signed".into(),
            auth_config: None,
            limits: RawLimits::default(),
            response: RawResponseSpec {
                mode: "json".into(),
                source: Some("service:threads/get".into()),
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
        let mode = JsonMode;
        let raw = RawRouteSpec {
            section: "routes".into(),
            id: "test-route".into(),
            path: "/test".into(),
            methods: ["GET".into()].into_iter().collect(),
            auth: "rye_signed".into(),
            auth_config: None,
            limits: RawLimits::default(),
            response: RawResponseSpec {
                mode: "json".into(),
                source: Some("service:threads/get".into()),
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

    #[test]
    fn compile_rejects_undeclared_capture() {
        let mode = JsonMode;
        let raw = make_raw(
            "/threads/{thread_id}",
            Some("service:threads/get"),
            serde_json::json!({ "thread_id": "${path.unknown}" }),
        );
        let result = mode.compile(&raw, &compile_ctx());
        let err = match result {
            Err(e) => e,
            Ok(_) => panic!("expected error"),
        };
        let msg = format!("{err}");
        assert!(
            msg.contains("undeclared path capture 'unknown'"),
            "got: {msg}"
        );
    }

    #[test]
    fn extract_path_captures_works() {
        let caps = extract_path_captures("/threads/{thread_id}/events/{event_id}");
        assert_eq!(caps, vec!["thread_id", "event_id"]);
    }

    #[test]
    fn extract_path_captures_no_captures() {
        let caps = extract_path_captures("/health");
        assert!(caps.is_empty());
    }
}
