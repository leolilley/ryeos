//! `event_stream` response mode — unified SSE gateway + subscription.
//!
//! Two strategies, selected at compile time by the `source` field:
//!
//! | Source value        | Strategy     | Description                                  |
//! |---------------------|-------------|----------------------------------------------|
//! | `"dispatch_launch"` | Gateway     | Body-driven: parses item_ref from request body, mints thread ID, dispatches via engine, streams events |
//! | `"thread_events"`   | Subscription| Path-driven: extracts thread_id from path capture, subscribes to existing thread, streams events with principal ownership check |
//!
//! Both strategies produce SSE frames. Both share lag-recovery and
//! post-launch drain logic. The compile-time bifurcation ensures each
//! strategy gets its own validation rules.

use std::sync::Arc;
use std::time::Duration;

use axum::response::sse::{KeepAlive, Sse};
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::Value;

use crate::dispatch_error::{RouteConfigError, RouteDispatchError};
use crate::routes::compile::{
    CompiledResponseMode, CompiledRoute, ResponseMode, RouteDispatchContext,
};
use crate::routes::invocation::{
    CompiledRouteInvocation, InvocationCheck, RouteInvocationContext, RouteInvocationOutput,
    RouteInvocationResult,
};
use crate::routes::raw::{RawRequestBody, RawRouteSpec};

// ── Shared constants ────────────────────────────────────────────────────
// (Constants used by compile functions are defined near their usage.)

// ── Compile-time strategy selection ─────────────────────────────────────

/// The compiled strategy selected at route-table build time.
enum EventStreamStrategy {
    /// Body-driven launch: mode parses body, invoker spawns dispatch + tails events.
    Gateway {
        invoker: Arc<dyn CompiledRouteInvocation>,
    },
    /// Path-driven subscription: mode extracts thread_id from captures,
    /// invoker checks ownership + subscribes to events.
    Subscription {
        invoker: Arc<dyn CompiledRouteInvocation>,
        thread_id_capture: String,
    },
}

pub struct EventStreamMode;

pub struct CompiledEventStreamMode {
    strategy: EventStreamStrategy,
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
    ) -> Result<Arc<dyn CompiledResponseMode>, RouteConfigError> {
        if raw.execute.is_some() {
            return Err(RouteConfigError::InvalidResponseSpec {
                id: raw.id.clone(),
                mode: "event_stream".into(),
                reason: "event_stream mode must not have a top-level 'execute' block".into(),
            });
        }

        let source = raw.response.source.as_deref().unwrap_or("");

        let strategy = match source {
            "dispatch_launch" => compile_gateway(raw)?,
            "thread_events" => compile_subscription(raw)?,
            "" => {
                return Err(RouteConfigError::InvalidResponseSpec {
                    id: raw.id.clone(),
                    mode: "event_stream".into(),
                    reason: "event_stream mode requires `response.source` \
                        (expected 'dispatch_launch' or 'thread_events')"
                        .into(),
                });
            }
            other => {
                return Err(RouteConfigError::InvalidSourceConfig {
                    id: raw.id.clone(),
                    src: other.into(),
                    reason: format!(
                        "unknown event_stream source '{other}'; \
                         expected 'dispatch_launch' or 'thread_events'"
                    ),
                });
            }
        };

        Ok(Arc::new(CompiledEventStreamMode { strategy }))
    }
}

// ── Gateway compile ─────────────────────────────────────────────────────

const GATEWAY_REQUIRED_AUTH: &str = "ryeos_signed";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawGatewaySourceConfig {
    keep_alive_secs: u64,
}

fn compile_gateway(raw: &RawRouteSpec) -> Result<EventStreamStrategy, RouteConfigError> {
    if raw.auth != GATEWAY_REQUIRED_AUTH {
        return Err(RouteConfigError::SourceAuthRequirement {
            id: raw.id.clone(),
            src: "dispatch_launch".into(),
            required: GATEWAY_REQUIRED_AUTH.into(),
            got: raw.auth.clone(),
        });
    }

    if raw.request.body != RawRequestBody::Json {
        return Err(RouteConfigError::InvalidSourceConfig {
            id: raw.id.clone(),
            src: "dispatch_launch".into(),
            reason: "dispatch_launch requires request.body = json".into(),
        });
    }

    let cfg: RawGatewaySourceConfig =
        serde_json::from_value(raw.response.source_config.clone()).map_err(|e| {
            RouteConfigError::InvalidSourceConfig {
                id: raw.id.clone(),
                src: "dispatch_launch".into(),
                reason: format!("invalid source_config: {e}"),
            }
        })?;

    if cfg.keep_alive_secs == 0 {
        return Err(RouteConfigError::InvalidSourceConfig {
            id: raw.id.clone(),
            src: "dispatch_launch".into(),
            reason: "keep_alive_secs must be > 0".into(),
        });
    }

    let invoker = Arc::new(
        crate::routes::invokers::gateway_stream_invocation::CompiledGatewayStreamInvocation {
            keep_alive_secs: cfg.keep_alive_secs,
        },
    );

    Ok(EventStreamStrategy::Gateway { invoker })
}

// ── Subscription compile ────────────────────────────────────────────────

const SUBSCRIPTION_REQUIRED_AUTH: &str = "ryeos_signed";

fn compile_subscription(raw: &RawRouteSpec) -> Result<EventStreamStrategy, RouteConfigError> {
    if raw.auth != SUBSCRIPTION_REQUIRED_AUTH {
        return Err(RouteConfigError::SourceAuthRequirement {
            id: raw.id.clone(),
            src: "thread_events".into(),
            required: SUBSCRIPTION_REQUIRED_AUTH.into(),
            got: raw.auth.clone(),
        });
    }

    let source_config = &raw.response.source_config;

    let thread_id_template = source_config
        .get("thread_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| RouteConfigError::InvalidSourceConfig {
            id: raw.id.clone(),
            src: "thread_events".into(),
            reason: "missing 'thread_id' in source_config".into(),
        })?;

    let capture_name = validate_and_extract_path_capture(
        thread_id_template,
        "thread_id",
        &raw.id,
    )?;

    let declared_captures = extract_path_captures(&raw.path);
    if !declared_captures.contains(&capture_name) {
        return Err(RouteConfigError::InvalidSourceConfig {
            id: raw.id.clone(),
            src: "thread_events".into(),
            reason: format!(
                "thread_id references undeclared path capture '{capture_name}'; \
                 route path declares: [{declared}]",
                declared = declared_captures
                    .iter()
                    .map(|c| format!("'{c}'"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        });
    }

    let keep_alive_secs = source_config
        .get("keep_alive_secs")
        .and_then(|v| v.as_u64())
        .unwrap_or(15);
    if keep_alive_secs == 0 {
        return Err(RouteConfigError::InvalidSourceConfig {
            id: raw.id.clone(),
            src: "thread_events".into(),
            reason: "keep_alive_secs must be > 0".into(),
        });
    }

    let invoker = Arc::new(
        crate::routes::invokers::subscription_stream_invocation::CompiledSubscriptionStreamInvocation {
            keep_alive_secs,
        },
    );

    Ok(EventStreamStrategy::Subscription {
        invoker,
        thread_id_capture: capture_name,
    })
}

/// Validate that `template` is a single `${path.<name>}` interpolation
/// and return the capture name.
fn validate_and_extract_path_capture(
    template: &str,
    field: &str,
    route_id: &str,
) -> Result<String, RouteConfigError> {
    let trimmed = template.trim();
    let prefix = "${path.";
    let suffix = "}";

    // Must contain ${...}
    if let Some(start) = trimmed.find("${") {
        if let Some(end_offset) = trimmed[start..].find('}') {
            let inner = &trimmed[start + 2..start + end_offset];
            if !inner.starts_with("path.") {
                return Err(RouteConfigError::InvalidSourceConfig {
                    id: route_id.into(),
                    src: "thread_events".into(),
                    reason: format!(
                        "{field} must use ${{path.<name>}} interpolation, got ${{{inner}}}"
                    ),
                });
            }
        } else {
            return Err(RouteConfigError::InvalidSourceConfig {
                id: route_id.into(),
                src: "thread_events".into(),
                reason: format!("{field} contains unterminated '${{...}}' template"),
            });
        }

        // Must be a single template (no second interpolation).
        let after_first = &trimmed[start + 2..];
        if after_first.find("${").is_some() {
            return Err(RouteConfigError::InvalidSourceConfig {
                id: route_id.into(),
                src: "thread_events".into(),
                reason: format!("{field} must be a single ${{path.<name>}} template"),
            });
        }
    } else {
        return Err(RouteConfigError::InvalidSourceConfig {
            id: route_id.into(),
            src: "thread_events".into(),
            reason: format!("{field} must use ${{path.<name>}} interpolation"),
        });
    }

    // Extract the capture name.
    if let Some(rest) = trimmed.strip_prefix(prefix) {
        if let Some(name) = rest.strip_suffix(suffix) {
            return Ok(name.to_string());
        }
    }

    Err(RouteConfigError::InvalidSourceConfig {
        id: route_id.into(),
        src: "thread_events".into(),
        reason: format!("{field} has invalid path capture template"),
    })
}

fn extract_path_captures(path: &str) -> std::collections::HashSet<String> {
    let mut captures = std::collections::HashSet::new();
    for segment in path.split('/').skip(1) {
        if let Some(name) = segment.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
            captures.insert(name.to_string());
        }
    }
    captures
}

// ── Dispatch (request-time) ─────────────────────────────────────────────

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
        compiled: &CompiledRoute,
        ctx: RouteDispatchContext,
    ) -> Result<axum::response::Response, RouteDispatchError> {
        // Prepare invocation input depending on strategy.
        let (invoker, input): (&Arc<dyn CompiledRouteInvocation>, Value) = match &self.strategy {
            EventStreamStrategy::Gateway { invoker } => {
                // Gateway: pass the raw body as input for the invoker to parse.
                let input = serde_json::from_slice(&ctx.body_raw).map_err(|e| {
                    RouteDispatchError::BadRequest(format!("invalid request body: {e}"))
                })?;
                (invoker, input)
            }
            EventStreamStrategy::Subscription {
                invoker,
                thread_id_capture,
            } => {
                // Subscription: extract thread_id from path captures.
                let thread_id = ctx
                    .captures
                    .get(thread_id_capture)
                    .ok_or_else(|| {
                        RouteDispatchError::Internal(format!(
                            "path capture '{}' not found in request",
                            thread_id_capture
                        ))
                    })?
                    .clone();
                let input = serde_json::json!({ "thread_id": thread_id });
                (invoker, input)
            }
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
            invoker.as_ref(),
            InvocationCheck {
                expected_output: RouteInvocationOutput::Stream,
            },
            inv_ctx,
        )
        .await?;

        match result {
            RouteInvocationResult::Stream(stream) => {
                let keep_alive_secs = stream.keep_alive_secs.max(1);
                let keep_alive = KeepAlive::new()
                    .interval(Duration::from_secs(keep_alive_secs))
                    .text(":");
                let sse = Sse::new(stream.events).keep_alive(keep_alive);
                Ok(sse.into_response())
            }
            // invoke_checked guarantees Stream.
            _ => unreachable!("invoke_checked enforces Stream"),
        }
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::invokers::gateway_stream_invocation::LaunchRequest;
    use crate::routes::invokers::stream_helpers::is_terminal_status;
    use crate::routes::invokers::subscription_stream_invocation::parse_last_event_id;
    use crate::routes::raw::{RawLimits, RawRequest, RawRequestBody, RawResponseSpec};

    fn compile_ctx() {}

    // ── Last-Event-ID tests ────────────────────

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

    // ── Compile-time validation ────────────────

    fn make_gateway_raw(id: &str, path: &str) -> RawRouteSpec {
        RawRouteSpec {
            section: "routes".into(),
            category: None,
            id: id.into(),
            path: path.into(),
            methods: ["POST".into()].into_iter().collect(),
            auth: "ryeos_signed".into(),
            auth_config: None,
            limits: RawLimits::default(),
            response: RawResponseSpec {
                mode: "event_stream".into(),
                source: Some("dispatch_launch".into()),
                source_config: serde_json::json!({
                    "keep_alive_secs": 15,
                }),
                status: None,
                content_type: None,
                body_b64: None,
            },
            execute: None,
            request: RawRequest {
                body: RawRequestBody::Json,
            },
            source_file: std::path::PathBuf::from(format!("/test/{id}.yaml")),
        }
    }

    fn make_subscription_raw(id: &str, path: &str) -> RawRouteSpec {
        RawRouteSpec {
            section: "routes".into(),
            category: None,
            id: id.into(),
            path: path.into(),
            methods: ["GET".into()].into_iter().collect(),
            auth: "ryeos_signed".into(),
            auth_config: None,
            limits: RawLimits::default(),
            response: RawResponseSpec {
                mode: "event_stream".into(),
                source: Some("thread_events".into()),
                source_config: serde_json::json!({
                    "thread_id": "${path.id}",
                    "keep_alive_secs": 15,
                }),
                status: None,
                content_type: None,
                body_b64: None,
            },
            execute: None,
            request: RawRequest {
                body: RawRequestBody::None,
            },
            source_file: std::path::PathBuf::from(format!("/test/{id}.yaml")),
        }
    }

    #[test]
    fn compile_gateway_succeeds() {
        let raw = make_gateway_raw("r1", "/execute/stream");
        let result = EventStreamMode.compile(&raw);
        assert!(result.is_ok(), "gateway compile should succeed");
    }

    #[test]
    fn compile_subscription_succeeds() {
        let raw = make_subscription_raw("r1", "/threads/{id}/stream");
        let result = EventStreamMode.compile(&raw);
        assert!(result.is_ok(), "subscription compile should succeed");
    }

    #[test]
    fn compile_rejects_execute_block() {
        use crate::routes::raw::RawExecute;
        let mut raw = make_gateway_raw("r1", "/execute/stream");
        raw.execute = Some(RawExecute {
            item_ref: "tool:x/y".into(),
            params: serde_json::Value::Null,
        });
        let result = EventStreamMode.compile(&raw);
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
        let mut raw = make_gateway_raw("r1", "/test/{id}");
        raw.response.source = Some("nonexistent_source".into());
        raw.response.source_config = serde_json::Value::Null;
        let result = EventStreamMode.compile(&raw);
        let err = match result {
            Err(e) => e,
            Ok(_) => panic!("expected error"),
        };
        let msg = format!("{err}");
        assert!(
            msg.contains("unknown event_stream source"),
            "got: {msg}"
        );
    }

    #[test]
    fn compile_rejects_missing_source() {
        let mut raw = make_gateway_raw("r1", "/test/{id}");
        raw.response.source = None;
        raw.response.source_config = serde_json::Value::Null;
        let result = EventStreamMode.compile(&raw);
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

    // ── Gateway-specific compile tests ─────────

    #[test]
    fn gateway_rejects_auth_none() {
        let mut raw = make_gateway_raw("r1", "/execute/stream");
        raw.auth = "none".into();
        let result = EventStreamMode.compile(&raw);
        let err = match result { Err(e) => e, Ok(_) => panic!("expected error") };
        let msg = format!("{err}");
        assert!(msg.contains("requires auth 'ryeos_signed'"), "got: {msg}");
    }

    #[test]
    fn gateway_rejects_non_json_body() {
        let mut raw = make_gateway_raw("r1", "/execute/stream");
        raw.request.body = RawRequestBody::Raw;
        let result = EventStreamMode.compile(&raw);
        let err = match result { Err(e) => e, Ok(_) => panic!("expected error") };
        let msg = format!("{err}");
        assert!(msg.contains("requires request.body = json"), "got: {msg}");
    }

    #[test]
    fn gateway_rejects_keep_alive_zero() {
        let mut raw = make_gateway_raw("r1", "/execute/stream");
        raw.response.source_config = serde_json::json!({"keep_alive_secs": 0});
        let result = EventStreamMode.compile(&raw);
        let err = match result { Err(e) => e, Ok(_) => panic!("expected error") };
        let msg = format!("{err}");
        assert!(msg.contains("keep_alive_secs must be > 0"), "got: {msg}");
    }

    #[test]
    fn gateway_rejects_unknown_source_config_key() {
        let mut raw = make_gateway_raw("r1", "/execute/stream");
        raw.response.source_config = serde_json::json!({
            "keep_alive_secs": 15,
            "item_ref": "bogus",
        });
        let result = EventStreamMode.compile(&raw);
        let err = match result { Err(e) => e, Ok(_) => panic!("expected error") };
        let msg = format!("{err}");
        assert!(
            msg.contains("unknown field") && msg.contains("item_ref"),
            "got: {msg}"
        );
    }

    #[test]
    fn gateway_rejects_missing_keep_alive_secs() {
        let mut raw = make_gateway_raw("r1", "/execute/stream");
        raw.response.source_config = serde_json::json!({});
        let result = EventStreamMode.compile(&raw);
        let err = match result { Err(e) => e, Ok(_) => panic!("expected error") };
        let msg = format!("{err}");
        assert!(
            msg.contains("missing field `keep_alive_secs`"),
            "got: {msg}"
        );
    }

    #[test]
    fn gateway_rejects_non_object_source_config() {
        let mut raw = make_gateway_raw("r1", "/execute/stream");
        raw.response.source_config = serde_json::json!(123);
        let result = EventStreamMode.compile(&raw);
        let err = match result { Err(e) => e, Ok(_) => panic!("expected error") };
        let msg = format!("{err}");
        assert!(msg.contains("invalid source_config"), "got: {msg}");
    }

    #[test]
    fn launch_request_rejects_missing_fields() {
        let body = serde_json::json!({"item_ref": "directive:foo"});
        let bytes = serde_json::to_vec(&body).unwrap();
        let err = serde_json::from_slice::<LaunchRequest>(&bytes)
            .expect_err("must reject missing fields");
        let msg = err.to_string();
        assert!(msg.contains("missing field"), "expected missing-field error, got: {msg}");
    }

    #[test]
    fn launch_request_rejects_unknown_field() {
        let body = serde_json::json!({
            "item_ref": "directive:foo",
            "project_path": "/tmp",
            "parameters": {},
            "extra": "nope",
        });
        let bytes = serde_json::to_vec(&body).unwrap();
        let err = serde_json::from_slice::<LaunchRequest>(&bytes)
            .expect_err("must reject unknown fields");
        let msg = err.to_string();
        assert!(
            msg.contains("unknown field") && msg.contains("extra"),
            "expected unknown-field error, got: {msg}"
        );
    }

    #[test]
    fn launch_request_accepts_complete_body() {
        let body = serde_json::json!({
            "item_ref": "directive:my/agent",
            "project_path": "/tmp/proj",
            "parameters": {"name": "World"},
        });
        let bytes = serde_json::to_vec(&body).unwrap();
        let req: LaunchRequest =
            serde_json::from_slice(&bytes).expect("valid body must parse");
        assert_eq!(req.item_ref, "directive:my/agent");
        assert_eq!(req.project_path, "/tmp/proj");
        assert_eq!(req.parameters["name"], "World");
    }

    // ── Subscription-specific compile tests ────

    #[test]
    fn subscription_rejects_auth_none() {
        let mut raw = make_subscription_raw("r1", "/threads/{id}/stream");
        raw.auth = "none".into();
        let result = EventStreamMode.compile(&raw);
        let err = match result { Err(e) => e, Ok(_) => panic!("expected error") };
        let msg = format!("{err}");
        assert!(
            msg.contains("requires auth 'ryeos_signed'"),
            "got: {msg}"
        );
    }

    #[test]
    fn subscription_rejects_non_path_interpolation() {
        let mut raw = make_subscription_raw("r1", "/threads/{id}/stream");
        raw.response.source_config = serde_json::json!({
            "thread_id": "${query.id}",
            "keep_alive_secs": 15,
        });
        let result = EventStreamMode.compile(&raw);
        let err = match result { Err(e) => e, Ok(_) => panic!("expected error") };
        let msg = format!("{err}");
        assert!(msg.contains("must use ${path."), "got: {msg}");
    }

    #[test]
    fn subscription_rejects_undeclared_capture() {
        let mut raw = make_subscription_raw("r1", "/threads/{tid}/stream");
        raw.response.source_config = serde_json::json!({
            "thread_id": "${path.wrong}",
            "keep_alive_secs": 15,
        });
        let result = EventStreamMode.compile(&raw);
        let err = match result { Err(e) => e, Ok(_) => panic!("expected error") };
        let msg = format!("{err}");
        assert!(msg.contains("undeclared path capture"), "got: {msg}");
    }

    #[test]
    fn subscription_rejects_keep_alive_zero() {
        let mut raw = make_subscription_raw("r1", "/threads/{id}/stream");
        raw.response.source_config = serde_json::json!({
            "thread_id": "${path.id}",
            "keep_alive_secs": 0,
        });
        let result = EventStreamMode.compile(&raw);
        let err = match result { Err(e) => e, Ok(_) => panic!("expected error") };
        let msg = format!("{err}");
        assert!(msg.contains("keep_alive_secs must be > 0"), "got: {msg}");
    }

    // ── Shared helpers ─────────────────────────

    #[test]
    fn is_terminal_status_detects_all() {
        assert!(is_terminal_status("completed"));
        assert!(is_terminal_status("failed"));
        assert!(is_terminal_status("cancelled"));
        assert!(is_terminal_status("killed"));
        assert!(is_terminal_status("timed_out"));
        assert!(!is_terminal_status("running"));
        assert!(!is_terminal_status("pending"));
    }

    #[test]
    fn extract_path_capture_valid() {
        assert_eq!(
            validate_and_extract_path_capture("${path.thread_id}", "thread_id", "r1").unwrap(),
            "thread_id"
        );
    }

    #[test]
    fn extract_path_capture_rejects_non_path() {
        assert!(validate_and_extract_path_capture("${query.id}", "thread_id", "r1").is_err());
    }

    #[test]
    fn extract_path_captures_finds_all() {
        let caps = extract_path_captures("/threads/{id}/events/{sub}");
        assert!(caps.contains("id"));
        assert!(caps.contains("sub"));
        assert_eq!(caps.len(), 2);
    }

    #[test]
    fn validate_rejects_double_interpolation() {
        assert!(
            validate_and_extract_path_capture("${path.x}-${path.y}", "f", "r1").is_err()
        );
    }

    #[test]
    fn validate_rejects_static_string() {
        assert!(
            validate_and_extract_path_capture("static", "f", "r1").is_err()
        );
    }
}
