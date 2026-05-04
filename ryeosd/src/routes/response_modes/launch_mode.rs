//! Unary `launch` response mode.
//!
//! Wire shape: HTTP request → verify auth → mint thread id → spawn
//! `dispatch::dispatch` in a detached background task with the
//! pre-minted id → return `202 Accepted` with `{ "thread_id": ... }`
//! synchronously. The launched thread keeps running after the HTTP
//! response is closed; durable lifecycle events flow through the
//! event store and can be tailed via the `dispatch_launch` SSE source
//! or the `thread_events` source on a separate route.
//!
//! The mode is **kind-agnostic**. The route YAML pins `item_ref`
//! (any canonical ref) and `project_path`; the daemon does not
//! pattern-match the kind name. The engine's kind-schema registry
//! decides root-executability inside `dispatch::dispatch`.
//!
//! Body→params shape: the request body is wrapped in a stable
//! [`LaunchEnvelope`] so directives have a single contract regardless
//! of request body format. JSON bodies surface as
//! `envelope.body = <parsed-Value>`; text bodies (form-encoded is
//! the canonical case) surface as `envelope.body = "<raw-utf8>"`.
//! `envelope.meta` carries verifier-supplied data — delivery id,
//! allow-listed forwarded headers, route id, verifier key — so a
//! single directive can route across event types without re-reading
//! the HTTP request. The daemon does not name vendors; if a route
//! YAML wants a vendor tag, it forwards an upstream header via
//! `forwarded_headers` (which surfaces under `meta.headers.<name>`)
//! or sets it via response-mode params at the directive layer.
//!
//! Compile-time validation rejects:
//! * top-level `execute` block (this mode uses `source_config`
//!   instead, so the ambiguity is loud)
//! * `response.source` (no streaming source — this is a unary mode)
//! * `response.status` / `content_type` / `body_b64` (those are
//!   `static`-mode fields)
//! * `request.body == none | raw` (must be `json` or `text`)
//! * unknown `source_config` keys
//! * `source_config.item_ref` that fails `CanonicalRef::parse`
//! * `source_config.project_path` that is not absolute
//!
//! No runtime fallbacks — every path is closed at load time.

use std::sync::Arc;

use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::Value;

use crate::dispatch_error::{RouteConfigError, RouteDispatchError};
use crate::routes::compile::{
    CompiledResponseMode, CompiledRoute, ModeCompileContext, ResponseMode, RouteDispatchContext,
};
use crate::routes::raw::{RawRequestBody, RawRouteSpec};

pub struct LaunchMode;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLaunchSourceConfig {
    item_ref: String,
    project_path: String,
}

pub struct CompiledLaunchMode {
    item_ref: crate::routes::parsed_ref::ParsedItemRef,
    project_path: crate::routes::abs_path::AbsolutePathBuf,
    body: RawRequestBody,
}

impl ResponseMode for LaunchMode {
    fn key(&self) -> &'static str {
        "launch"
    }

    fn compile(
        &self,
        raw: &RawRouteSpec,
        _ctx: &ModeCompileContext,
    ) -> Result<Arc<dyn CompiledResponseMode>, RouteConfigError> {
        if raw.execute.is_some() {
            return Err(RouteConfigError::InvalidResponseSpec {
                id: raw.id.clone(),
                mode: "launch".into(),
                reason: "launch mode must not have a top-level 'execute' block; \
                    pin item_ref + project_path under response.source_config"
                    .into(),
            });
        }
        if raw.response.source.is_some() {
            return Err(RouteConfigError::InvalidResponseSpec {
                id: raw.id.clone(),
                mode: "launch".into(),
                reason: "launch mode must not declare a streaming `response.source`".into(),
            });
        }
        if raw.response.status.is_some()
            || raw.response.content_type.is_some()
            || raw.response.body_b64.is_some()
        {
            return Err(RouteConfigError::InvalidResponseSpec {
                id: raw.id.clone(),
                mode: "launch".into(),
                reason: "launch mode must not set static-mode fields \
                    (status / content_type / body_b64)"
                    .into(),
            });
        }

        match raw.request.body {
            RawRequestBody::Json | RawRequestBody::Text => {}
            RawRequestBody::None | RawRequestBody::Raw => {
                return Err(RouteConfigError::InvalidResponseSpec {
                    id: raw.id.clone(),
                    mode: "launch".into(),
                    reason: format!(
                        "launch mode requires request.body = json | text; got {:?}",
                        raw.request.body
                    ),
                });
            }
        }

        let cfg: RawLaunchSourceConfig = if raw.response.source_config.is_null() {
            return Err(RouteConfigError::InvalidResponseSpec {
                id: raw.id.clone(),
                mode: "launch".into(),
                reason: "launch mode requires response.source_config = \
                    { item_ref, project_path }"
                    .into(),
            });
        } else {
            serde_json::from_value(raw.response.source_config.clone()).map_err(|e| {
                RouteConfigError::InvalidResponseSpec {
                    id: raw.id.clone(),
                    mode: "launch".into(),
                    reason: format!("invalid source_config: {e}"),
                }
            })?
        };

        // Syntactic ref validation: parse the canonical ref shape
        // (`<kind>:<id>`). The daemon does NOT check the kind name
        // here; whether the kind is root-executable is decided by
        // the engine's kind-schema registry inside
        // `dispatch::dispatch`.
        let item_ref = crate::routes::parsed_ref::ParsedItemRef::parse(&cfg.item_ref).map_err(|e| {
            RouteConfigError::InvalidResponseSpec {
                id: raw.id.clone(),
                mode: "launch".into(),
                reason: format!(
                    "source_config.item_ref '{}' is not a valid canonical ref: {e}",
                    cfg.item_ref
                ),
            }
        })?;

        let project_path =
            crate::routes::abs_path::AbsolutePathBuf::try_from_str(&cfg.project_path).map_err(
                |e| RouteConfigError::InvalidResponseSpec {
                    id: raw.id.clone(),
                    mode: "launch".into(),
                    reason: format!("source_config.{}", e),
                },
            )?;

        Ok(Arc::new(CompiledLaunchMode {
            item_ref,
            project_path,
            body: raw.request.body.clone(),
        }))
    }
}

#[axum::async_trait]
impl CompiledResponseMode for CompiledLaunchMode {
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
        // Parse body per declared shape.
        let body_value: Value = match self.body {
            RawRequestBody::Json => serde_json::from_slice(&ctx.body_raw).map_err(|e| {
                RouteDispatchError::BadRequest(format!("invalid JSON body: {e}"))
            })?,
            RawRequestBody::Text => {
                let s = std::str::from_utf8(&ctx.body_raw).map_err(|e| {
                    RouteDispatchError::BadRequest(format!("body is not valid UTF-8: {e}"))
                })?;
                Value::String(s.to_string())
            }
            // Compile-time check rejects None | Raw, but be defensive.
            RawRequestBody::None | RawRequestBody::Raw => {
                return Err(RouteDispatchError::Internal(
                    "launch mode reached handle() with non-json/text body".into(),
                ));
            }
        };

        let received_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .map_err(|e| {
                RouteDispatchError::Internal(format!("system clock failure: {e}"))
            })?;

        // Build the launch envelope. `meta` is a stable kind-agnostic
        // shape (`delivery_id`, `headers`, `route_id`, `verifier`,
        // `principal_id`, `received_at_unix`); directives that don't
        // care about meta just read `body`. No vendor names appear
        // here — the daemon never tags requests by vendor.
        let envelope = build_launch_envelope(
            body_value,
            compiled.id.as_str(),
            &ctx.principal,
            received_at,
        );

        let thread_id = crate::services::thread_lifecycle::new_thread_id();

        let span = tracing::info_span!(
            "launch_mode",
            route_id = compiled.id.as_str(),
            thread_id = thread_id.as_str(),
            verifier = ctx.principal.verifier_key,
            principal_id = ctx.principal.id.as_str(),
            delivery_id = ctx
                .principal
                .metadata
                .get("delivery_id")
                .map(String::as_str)
                .unwrap_or(""),
        );
        let _enter = span.enter();

        let handle = crate::routes::launch::spawn_dispatch_launch(
            &ctx.state,
            self.item_ref.clone(),
            self.project_path.clone(),
            envelope,
            ctx.principal.id.clone(),
            ctx.principal.scopes.clone(),
            thread_id.clone(),
        );

        // Detached watcher: do not silently drop the JoinHandle.
        // Awaits the handle and logs the outcome at warn (dispatch
        // error) or error (panic) level. Tracing alone is the
        // observability surface for now — see roadmap E.2 for
        // metric integration.
        let watch_route_id = compiled.id.clone();
        let watch_thread_id = thread_id.clone();
        tokio::spawn(async move {
            match handle.await {
                Ok(Ok(())) => {
                    tracing::debug!(
                        route_id = %watch_route_id,
                        thread_id = %watch_thread_id,
                        "launch mode background dispatch completed"
                    );
                }
                Ok(Err(e)) => {
                    tracing::warn!(
                        route_id = %watch_route_id,
                        thread_id = %watch_thread_id,
                        code = %e.code(),
                        error = %e,
                        "launch mode background dispatch failed"
                    );
                }
                Err(join_err) => {
                    tracing::error!(
                        route_id = %watch_route_id,
                        thread_id = %watch_thread_id,
                        error = %join_err,
                        "launch mode background dispatch panicked"
                    );
                }
            }
        });

        let body =
            axum::Json(serde_json::json!({ "thread_id": thread_id })).into_response();
        let mut resp = body;
        *resp.status_mut() = StatusCode::ACCEPTED;
        Ok(resp)
    }
}

/// Build the stable launch envelope passed to the directive as
/// `params`. Shape:
/// ```json
/// {
///   "body": <parsed-body>,
///   "meta": {
///     "delivery_id": "<verifier-supplied, optional>",
///     "headers": { "<lowercase-name>": "<value>", ... },
///     "verifier": "<verifier key>",
///     "principal_id": "<verifier-supplied>",
///     "route_id": "<route id>",
///     "received_at_unix": <u64>
///   }
/// }
/// ```
/// The shape is kind-agnostic: there is no vendor-tag field, and the
/// daemon never invents one. `headers` only contains entries the
/// verifier explicitly allow-listed (see `hmac::forwarded_headers`);
/// arbitrary request headers are NEVER reflected into directive
/// params. `delivery_id` is omitted when the verifier did not extract
/// one.
fn build_launch_envelope(
    body: Value,
    route_id: &str,
    principal: &crate::routes::compile::RoutePrincipal,
    received_at_unix: u64,
) -> Value {
    let mut headers = serde_json::Map::new();
    let mut delivery_id = "";
    for (k, v) in &principal.metadata {
        if let Some(name) = k.strip_prefix("header.") {
            headers.insert(name.to_string(), Value::String(v.clone()));
        } else if k == "delivery_id" {
            delivery_id = v.as_str();
        }
    }

    let mut meta = serde_json::Map::new();
    if !delivery_id.is_empty() {
        meta.insert("delivery_id".into(), Value::String(delivery_id.into()));
    }
    meta.insert("headers".into(), Value::Object(headers));
    meta.insert("verifier".into(), Value::String(principal.verifier_key.into()));
    meta.insert("principal_id".into(), Value::String(principal.id.clone()));
    meta.insert("route_id".into(), Value::String(route_id.into()));
    meta.insert(
        "received_at_unix".into(),
        Value::Number(received_at_unix.into()),
    );

    let mut envelope = serde_json::Map::new();
    envelope.insert("body".into(), body);
    envelope.insert("meta".into(), Value::Object(meta));
    Value::Object(envelope)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::compile::RoutePrincipal;
    use crate::routes::raw::{RawExecute, RawLimits, RawRequest, RawRequestBody, RawResponseSpec};

    fn make_raw(id: &str) -> RawRouteSpec {
        RawRouteSpec {
            section: "routes".into(),
            id: id.into(),
            path: "/hook/test".into(),
            methods: ["POST".into()].into_iter().collect(),
            auth: "hmac".into(),
            auth_config: None,
            limits: RawLimits::default(),
            response: RawResponseSpec {
                mode: "launch".into(),
                source: None,
                source_config: serde_json::json!({
                    "item_ref": "directive:webhooks/handler",
                    "project_path": "/opt/proj",
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

    fn ctx() -> ModeCompileContext<'static> {
        ModeCompileContext {
            _phantom: std::marker::PhantomData,
        }
    }

    #[test]
    fn compile_succeeds_on_valid_route() {
        let raw = make_raw("r1");
        let _ = LaunchMode.compile(&raw, &ctx()).expect("valid launch route compiles");
    }

    #[test]
    fn compile_rejects_execute_block() {
        let mut raw = make_raw("r1");
        raw.execute = Some(RawExecute {
            item_ref: "directive:x".into(),
            params: Value::Null,
        });
        let err = match LaunchMode.compile(&raw, &ctx()) {
            Err(e) => e,
            Ok(_) => panic!("expected error"),
        };
        let msg = format!("{err}");
        assert!(msg.contains("must not have a top-level 'execute'"), "got: {msg}");
    }

    #[test]
    fn compile_rejects_response_source() {
        let mut raw = make_raw("r1");
        raw.response.source = Some("dispatch_launch".into());
        let err = match LaunchMode.compile(&raw, &ctx()) {
            Err(e) => e,
            Ok(_) => panic!("expected error"),
        };
        let msg = format!("{err}");
        assert!(msg.contains("must not declare a streaming `response.source`"), "got: {msg}");
    }

    #[test]
    fn compile_rejects_static_mode_fields() {
        let mut raw = make_raw("r1");
        raw.response.status = Some(204);
        let err = match LaunchMode.compile(&raw, &ctx()) {
            Err(e) => e,
            Ok(_) => panic!("expected error"),
        };
        let msg = format!("{err}");
        assert!(msg.contains("static-mode fields"), "got: {msg}");
    }

    #[test]
    fn compile_rejects_body_none() {
        let mut raw = make_raw("r1");
        raw.request.body = RawRequestBody::None;
        let err = match LaunchMode.compile(&raw, &ctx()) {
            Err(e) => e,
            Ok(_) => panic!("expected error"),
        };
        let msg = format!("{err}");
        assert!(msg.contains("requires request.body = json | text"), "got: {msg}");
    }

    #[test]
    fn compile_rejects_body_raw() {
        let mut raw = make_raw("r1");
        raw.request.body = RawRequestBody::Raw;
        let err = match LaunchMode.compile(&raw, &ctx()) {
            Err(e) => e,
            Ok(_) => panic!("expected error"),
        };
        let msg = format!("{err}");
        assert!(msg.contains("requires request.body = json | text"), "got: {msg}");
    }

    #[test]
    fn compile_accepts_body_text() {
        let mut raw = make_raw("r1");
        raw.request.body = RawRequestBody::Text;
        let _ = LaunchMode.compile(&raw, &ctx()).expect("text body must be accepted");
    }

    #[test]
    fn compile_rejects_missing_source_config() {
        let mut raw = make_raw("r1");
        raw.response.source_config = Value::Null;
        let err = match LaunchMode.compile(&raw, &ctx()) {
            Err(e) => e,
            Ok(_) => panic!("expected error"),
        };
        let msg = format!("{err}");
        assert!(msg.contains("requires response.source_config"), "got: {msg}");
    }

    #[test]
    fn compile_rejects_unknown_source_config_key() {
        let mut raw = make_raw("r1");
        raw.response.source_config = serde_json::json!({
            "item_ref": "directive:x",
            "project_path": "/opt/p",
            "stranger": "danger",
        });
        let err = match LaunchMode.compile(&raw, &ctx()) {
            Err(e) => e,
            Ok(_) => panic!("expected error"),
        };
        let msg = format!("{err}");
        assert!(msg.contains("unknown field"), "got: {msg}");
    }

    #[test]
    fn compile_rejects_invalid_canonical_ref() {
        let mut raw = make_raw("r1");
        raw.response.source_config = serde_json::json!({
            "item_ref": "no-colon-here",
            "project_path": "/opt/p",
        });
        let err = match LaunchMode.compile(&raw, &ctx()) {
            Err(e) => e,
            Ok(_) => panic!("expected error"),
        };
        let msg = format!("{err}");
        assert!(msg.contains("not a valid canonical ref"), "got: {msg}");
    }

    #[test]
    fn compile_accepts_any_kind_in_canonical_ref() {
        // Daemon must NOT pattern-match the kind name. tool:, graph:,
        // service:, anything that parses as a canonical ref is
        // syntactically valid here. Whether it's root-executable is
        // the engine's call at dispatch time.
        for ref_str in &[
            "directive:webhooks/handler",
            "tool:foo/bar",
            "graph:flow/a",
            "service:billing",
            "fictional_kind:anything/here",
        ] {
            let mut raw = make_raw("r1");
            raw.response.source_config = serde_json::json!({
                "item_ref": ref_str,
                "project_path": "/opt/p",
            });
            let r = LaunchMode.compile(&raw, &ctx());
            assert!(r.is_ok(), "ref '{}' should compile, got: {:?}", ref_str, r.err());
        }
    }

    #[test]
    fn compile_rejects_relative_project_path() {
        let mut raw = make_raw("r1");
        raw.response.source_config = serde_json::json!({
            "item_ref": "directive:x",
            "project_path": "relative/path",
        });
        let err = match LaunchMode.compile(&raw, &ctx()) {
            Err(e) => e,
            Ok(_) => panic!("expected error"),
        };
        let msg = format!("{err}");
        assert!(msg.contains("must be absolute"), "got: {msg}");
    }

    #[test]
    fn build_launch_envelope_shapes_meta() {
        let mut metadata = std::collections::BTreeMap::new();
        metadata.insert("delivery_id".into(), "abc-123".into());
        metadata.insert("header.x-github-event".into(), "push".into());
        metadata.insert("header.x-github-delivery".into(), "abc-123".into());
        let principal = RoutePrincipal {
            id: "webhook:hmac:r1".into(),
            scopes: vec![],
            verifier_key: "hmac",
            verified: true,
            metadata,
        };
        let body = serde_json::json!({"action": "opened"});
        let env = build_launch_envelope(body, "r1", &principal, 1_700_000_000);

        // Body passes through verbatim.
        assert_eq!(env["body"]["action"], "opened");

        // Meta carries delivery_id + route + verifier + principal + ts.
        // No `provider` field — the daemon does not name vendors.
        assert!(env["meta"].get("provider").is_none());
        assert_eq!(env["meta"]["delivery_id"], "abc-123");
        assert_eq!(env["meta"]["route_id"], "r1");
        assert_eq!(env["meta"]["verifier"], "hmac");
        assert_eq!(env["meta"]["principal_id"], "webhook:hmac:r1");
        assert_eq!(env["meta"]["received_at_unix"], 1_700_000_000);

        // Headers grouped under meta.headers (verifier-allowlisted only).
        assert_eq!(env["meta"]["headers"]["x-github-event"], "push");
        assert_eq!(env["meta"]["headers"]["x-github-delivery"], "abc-123");
    }

    #[test]
    fn build_launch_envelope_omits_missing_delivery() {
        let principal = RoutePrincipal::anonymous("none:r1".into(), "none");
        let body = serde_json::json!("hi");
        let env = build_launch_envelope(body, "r1", &principal, 0);
        assert!(env["meta"].get("provider").is_none());
        assert!(env["meta"].get("delivery_id").is_none());
        assert_eq!(env["meta"]["headers"], serde_json::json!({}));
        assert_eq!(env["body"], "hi");
    }
}
