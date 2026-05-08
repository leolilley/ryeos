//! `json` response mode — synchronous JSON from any canonical ref.
//!
//! The mode holds a compiled `CompiledRouteInvocation` that resolves to a
//! `Json` result. At dispatch time it interpolates `source_config`, calls
//! the invoker, and frames the result as HTTP JSON (200 or 404).
//!
//! Supported source kinds:
//! - `service:` — in-process handler (flat source_config as params)
//! - `tool:` / `directive:` / `graph:` — engine dispatch
//!   (source_config must have `{ project_path, parameters }`)

use std::sync::Arc;

use axum::http::StatusCode;
use axum::response::IntoResponse;

use crate::dispatch_error::{RouteConfigError, RouteDispatchError};
use crate::routes::compile::{CompiledResponseMode, CompiledRoute, ResponseMode, RouteDispatchContext};
use crate::routes::interpolation;
use crate::routes::invocation::{
    CompiledRouteInvocation, InvocationCheck, RouteInvocationContext,
    RouteInvocationOutput, RouteInvocationResult,
};
use crate::routes::raw::RawRouteSpec;

pub struct JsonMode;

/// Compiled state for a `json` mode route.
pub struct CompiledJsonMode {
    /// The compiled service invoker.
    invoker: Arc<dyn CompiledRouteInvocation>,
    /// The source_config template for runtime interpolation.
    source_config_template: serde_json::Value,
}

impl ResponseMode for JsonMode {
    fn key(&self) -> &'static str {
        "json"
    }

    fn compile(
        &self,
        raw: &RawRouteSpec,
    ) -> Result<Arc<dyn CompiledResponseMode>, RouteConfigError> {
        if raw.execute.is_some() {
            return Err(RouteConfigError::InvalidResponseSpec {
                id: raw.id.clone(),
                mode: "json".into(),
                reason: "json mode must not have a top-level 'execute' block".into(),
            });
        }

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

        // Compile the invoker via the universal canonical-ref compiler.
        let invoker = crate::routes::invokers::compile_canonical_ref_invoker(
            source_str, &raw.id,
        )?;

        // Non-service sources require typed { project_path, parameters } config.
        let parsed_kind = crate::routes::parsed_ref::ParsedItemRef::parse(source_str)
            .map(|r| r.kind().to_string())
            .unwrap_or_default();
        if parsed_kind != "service" {
            let config_obj = raw.response.source_config.as_object();
            match config_obj {
                None => {
                    return Err(RouteConfigError::InvalidSourceConfig {
                        id: raw.id.clone(),
                        src: source_str.into(),
                        reason: "non-service json source requires source_config with \
                            { project_path: \"...\", parameters: { ... } }"
                            .into(),
                    });
                }
                Some(obj) => {
                    if !obj.contains_key("project_path") {
                        return Err(RouteConfigError::InvalidSourceConfig {
                            id: raw.id.clone(),
                            src: source_str.into(),
                            reason: "non-service json source requires \
                                'project_path' in source_config"
                                .into(),
                        });
                    }
                }
            }
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
            invoker,
            source_config_template: raw.response.source_config.clone(),
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
        compiled: &CompiledRoute,
        ctx: RouteDispatchContext,
    ) -> Result<axum::response::Response, RouteDispatchError> {
        // Interpolate source_config templates with actual capture values.
        let input = if self.source_config_template.is_null() {
            serde_json::Value::Object(serde_json::Map::new())
        } else {
            interpolation::interpolate_path(&self.source_config_template, &ctx.captures)?
        };

        let inv_ctx = RouteInvocationContext {
            route_id: compiled.id.clone().into(),
            method: ctx.request_parts.method,
            uri: ctx.request_parts.uri,
            captures: std::collections::BTreeMap::from_iter(ctx.captures),
            headers: ctx.request_parts.headers,
            body_raw: ctx.body_raw,
            input,
            principal: Some(ctx.principal),
            state: ctx.state,
        };

        let result = crate::routes::invocation::invoke_checked(
            self.invoker.as_ref(),
            InvocationCheck {
                expected_output: RouteInvocationOutput::Json,
            },
            inv_ctx,
        )
        .await?;

        match result {
            RouteInvocationResult::Json(value) => {
                if value.is_null() {
                    Ok((
                        StatusCode::NOT_FOUND,
                        axum::Json(serde_json::json!({ "error": "not found" })),
                    )
                        .into_response())
                } else {
                    Ok(axum::Json(value).into_response())
                }
            }
            // invoke_checked guarantees Json; any other variant is already
            // caught as an Internal error by the enforcement layer.
            _ => unreachable!("invoke_checked enforces Json"),
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
    use crate::routes::raw::{RawLimits, RawRequest, RawRequestBody, RawResponseSpec};

    fn make_raw(
        path: &str,
        source: Option<&str>,
        source_config: serde_json::Value,
    ) -> RawRouteSpec {
        RawRouteSpec {
            section: "routes".into(),
            category: None,
            id: "test-route".into(),
            path: path.into(),
            methods: ["GET".into()].into_iter().collect(),
            auth: "ryeos_signed".into(),
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
        let result = mode.compile(&raw);
        assert!(result.is_ok(), "expected Ok, got: {:?}", result.err());
    }

    #[test]
    fn compile_rejects_missing_source() {
        let mode = JsonMode;
        let raw = make_raw("/test", None, serde_json::Value::Null);
        let result = mode.compile(&raw);
        let err = match result {
            Err(e) => e,
            Ok(_) => panic!("expected error"),
        };
        let msg = format!("{err}");
        assert!(msg.contains("requires `response.source`"), "got: {msg}");
    }

    #[test]
    fn compile_accepts_tool_kind_with_project_path() {
        let mode = JsonMode;
        let raw = make_raw(
            "/test",
            Some("tool:ryeos/core/execute"),
            serde_json::json!({ "project_path": "/tmp/test", "parameters": {} }),
        );
        let result = mode.compile(&raw);
        assert!(result.is_ok(), "tool: with project_path should be accepted, got: {:?}", result.err());
    }

    #[test]
    fn compile_accepts_directive_kind_with_project_path() {
        let mode = JsonMode;
        let raw = make_raw(
            "/test",
            Some("directive:my/agent"),
            serde_json::json!({ "project_path": "/tmp/test", "parameters": { "input": "val" } }),
        );
        let result = mode.compile(&raw);
        assert!(result.is_ok(), "directive: with project_path should be accepted, got: {:?}", result.err());
    }

    #[test]
    fn compile_accepts_graph_kind_with_project_path() {
        let mode = JsonMode;
        let raw = make_raw(
            "/test",
            Some("graph:workflows/approval"),
            serde_json::json!({ "project_path": "/tmp/test", "parameters": {} }),
        );
        let result = mode.compile(&raw);
        assert!(result.is_ok(), "graph: with project_path should be accepted, got: {:?}", result.err());
    }

    #[test]
    fn compile_rejects_tool_without_project_path() {
        let mode = JsonMode;
        let raw = make_raw(
            "/test",
            Some("tool:ryeos/core/execute"),
            serde_json::json!({ "parameters": {} }),
        );
        let result = mode.compile(&raw);
        let err = match result {
            Err(e) => e,
            Ok(_) => panic!("expected error for missing project_path"),
        };
        let msg = format!("{err}");
        assert!(msg.contains("project_path"), "got: {msg}");
    }

    #[test]
    fn compile_rejects_directive_with_null_source_config() {
        let mode = JsonMode;
        let raw = make_raw(
            "/test",
            Some("directive:my/agent"),
            serde_json::Value::Null,
        );
        let result = mode.compile(&raw);
        let err = match result {
            Err(e) => e,
            Ok(_) => panic!("expected error for null source_config"),
        };
        let msg = format!("{err}");
        assert!(msg.contains("project_path"), "got: {msg}");
    }

    #[test]
    fn compile_rejects_unknown_kind() {
        let mode = JsonMode;
        let raw = make_raw(
            "/test",
            Some("fictional:item/path"),
            serde_json::Value::Null,
        );
        let result = mode.compile(&raw);
        let err = match result {
            Err(e) => e,
            Ok(_) => panic!("expected error"),
        };
        let msg = format!("{err}");
        assert!(msg.contains("unsupported source kind"), "got: {msg}");
    }

    #[test]
    fn compile_rejects_unknown_service() {
        let mode = JsonMode;
        let raw = make_raw(
            "/test",
            Some("service:nonexistent/handler"),
            serde_json::Value::Null,
        );
        let result = mode.compile(&raw);
        let err = match result {
            Err(e) => e,
            Ok(_) => panic!("expected error"),
        };
        let msg = format!("{err}");
        assert!(msg.contains("no service descriptor found"), "got: {msg}");
    }

    #[test]
    fn compile_rejects_static_fields() {
        use crate::routes::raw::{RawLimits, RawRequest, RawRequestBody, RawResponseSpec};
        let mode = JsonMode;
        let raw = RawRouteSpec {
            section: "routes".into(),
            category: None,
            id: "test-route".into(),
            path: "/test".into(),
            methods: ["GET".into()].into_iter().collect(),
            auth: "ryeos_signed".into(),
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
        let result = mode.compile(&raw);
        let err = match result {
            Err(e) => e,
            Ok(_) => panic!("expected error"),
        };
        let msg = format!("{err}");
        assert!(msg.contains("must not set static-mode fields"), "got: {msg}");
    }

    #[test]
    fn compile_rejects_execute_block() {
        use crate::routes::raw::{RawExecute, RawLimits, RawRequest, RawRequestBody, RawResponseSpec};
        let mode = JsonMode;
        let raw = RawRouteSpec {
            section: "routes".into(),
            category: None,
            id: "test-route".into(),
            path: "/test".into(),
            methods: ["GET".into()].into_iter().collect(),
            auth: "ryeos_signed".into(),
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
        let result = mode.compile(&raw);
        let err = match result {
            Err(e) => e,
            Ok(_) => panic!("expected error"),
        };
        let msg = format!("{err}");
        assert!(msg.contains("must not have a top-level 'execute' block"), "got: {msg}");
    }

    #[test]
    fn compile_rejects_undeclared_capture() {
        let mode = JsonMode;
        let raw = make_raw(
            "/threads/{thread_id}",
            Some("service:threads/get"),
            serde_json::json!({ "thread_id": "${path.unknown}" }),
        );
        let result = mode.compile(&raw);
        let err = match result {
            Err(e) => e,
            Ok(_) => panic!("expected error"),
        };
        let msg = format!("{err}");
        assert!(msg.contains("undeclared path capture 'unknown'"), "got: {msg}");
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
