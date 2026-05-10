//! `execute` response mode — data-driven `/execute` route.
//!
//! This mode is the sole entry point for the `/execute` endpoint. The old
//! The old standalone `/execute` handler is deleted. All execute logic
//! lives here, driven by the dispatcher's per-route auth chain.
//!
//! Compile-time validation:
//! * `auth` must be `ryeos_signed`
//! * `request.body` must be `json`
//! * rejects `execute` block, `response.source`, static-mode fields
//!
//! Dispatch time:
//! 1. Principal comes from `ctx.principal` (set by the auth invoker).
//! 2. Body parsed as `ExecuteRequest`.
//! 3. Scope check (requires "execute").
//! 4. Full dispatch pipeline (token resolution, project source, engine dispatch).

use std::sync::Arc;

use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::dispatch_error::{RouteConfigError, RouteDispatchError};
use crate::execution::project_source::{self, ProjectSource, TempDirGuard};
use crate::routes::compile::{CompiledResponseMode, CompiledRoute, ResponseMode, RouteDispatchContext};
use crate::routes::raw::{RawRequestBody, RawRouteSpec};

// ── Request shape ─────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecuteRequest {
    /// Canonical item ref to execute (e.g. "directive:my/agent").
    /// Mutually exclusive with `tokens`.
    #[serde(default)]
    pub item_ref: Option<String>,
    /// CLI token sequence to resolve via alias registry.
    #[serde(default)]
    pub tokens: Option<Vec<String>>,
    /// Project root path for three-tier resolution.
    #[serde(default)]
    pub project_path: Option<String>,
    #[serde(default)]
    pub parameters: Value,
    #[serde(default = "default_launch_mode")]
    pub launch_mode: String,
    #[serde(default)]
    pub target_site_id: Option<String>,
    #[serde(default)]
    pub validate_only: bool,
    #[serde(default)]
    pub project_source: Option<ProjectSource>,
    #[serde(default)]
    pub operation: Option<String>,
    #[serde(default)]
    pub inputs: Option<Value>,
}

fn default_launch_mode() -> String {
    "inline".to_string()
}

// ── Mode ──────────────────────────────────────────────────────────────────

pub struct ExecuteMode;

pub struct CompiledExecuteMode;

impl ResponseMode for ExecuteMode {
    fn key(&self) -> &'static str {
        "execute"
    }

    fn compile(
        &self,
        raw: &RawRouteSpec,
    ) -> Result<Arc<dyn CompiledResponseMode>, RouteConfigError> {
        if raw.auth != "ryeos_signed" {
            return Err(RouteConfigError::InvalidResponseSpec {
                id: raw.id.clone(),
                mode: "execute".into(),
                reason: format!(
                    "execute mode requires auth = 'ryeos_signed'; got '{}'",
                    raw.auth
                ),
            });
        }

        if raw.execute.is_some() {
            return Err(RouteConfigError::InvalidResponseSpec {
                id: raw.id.clone(),
                mode: "execute".into(),
                reason: "execute mode must not have a top-level 'execute' block".into(),
            });
        }

        if raw.response.source.is_some() {
            return Err(RouteConfigError::InvalidResponseSpec {
                id: raw.id.clone(),
                mode: "execute".into(),
                reason: "execute mode must not declare response.source".into(),
            });
        }

        if raw.response.status.is_some()
            || raw.response.content_type.is_some()
            || raw.response.body_b64.is_some()
        {
            return Err(RouteConfigError::InvalidResponseSpec {
                id: raw.id.clone(),
                mode: "execute".into(),
                reason: "execute mode must not set static-mode fields \
                    (status / content_type / body_b64)"
                    .into(),
            });
        }

        match raw.request.body {
            RawRequestBody::Json => {}
            _ => {
                return Err(RouteConfigError::InvalidResponseSpec {
                    id: raw.id.clone(),
                    mode: "execute".into(),
                    reason: format!(
                        "execute mode requires request.body = json; got {:?}",
                        raw.request.body
                    ),
                });
            }
        }

        Ok(Arc::new(CompiledExecuteMode))
    }
}

#[axum::async_trait]
impl CompiledResponseMode for CompiledExecuteMode {
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
        let state = ctx.state;
        let principal = ctx.principal;

        // Principal is guaranteed present because auth = ryeos_signed.
        let caller_principal_id = principal.id.clone();
        let caller_scopes = principal.scopes.clone();

        // Scope check.
        if !caller_scopes.iter().any(|s| s == "*" || s == "execute") {
            return Ok((
                StatusCode::FORBIDDEN,
                axum::Json(json!({
                    "error": format!("insufficient scope: 'execute' required"),
                })),
            )
                .into_response());
        }

        // Parse body.
        let mut request: ExecuteRequest = serde_json::from_slice(&ctx.body_raw)
            .map_err(|e| RouteDispatchError::BadRequest(format!("invalid JSON body: {e}")))?;

        // API invariants: item_ref and tokens are mutually exclusive; one required.
        match (&request.item_ref, &request.tokens) {
            (Some(_), Some(_)) => {
                return Ok((
                    StatusCode::BAD_REQUEST,
                    axum::Json(json!({ "error": "item_ref and tokens are mutually exclusive" })),
                )
                    .into_response());
            }
            (None, None) => {
                return Ok((
                    StatusCode::BAD_REQUEST,
                    axum::Json(json!({ "error": "either item_ref or tokens is required" })),
                )
                    .into_response());
            }
            _ => {}
        }

        // Token mode: parameters must be empty.
        if request.tokens.is_some()
            && !request.parameters.is_null()
            && !request
                .parameters
                .as_object()
                .map_or(true, |m| m.is_empty())
        {
            return Ok((
                StatusCode::BAD_REQUEST,
                axum::Json(json!({
                    "error": "in tokens mode, parameters are computed server-side; client must not supply them"
                })),
            )
                .into_response());
        }

        // Token resolution.
        if let Some(ref tokens) = request.tokens {
            if tokens.is_empty() {
                return Ok((
                    StatusCode::BAD_REQUEST,
                    axum::Json(json!({ "error": "tokens array is empty" })),
                )
                    .into_response());
            }
            let resolved = ryeos_runtime::resolve_command(
                tokens,
                &state.alias_registry,
                &state.verb_registry,
            )
            .map_err(|e| match &e {
                ryeos_runtime::ResolveError::NoMatch { tokens } => {
                    RouteDispatchError::BadRequest(json!({
                        "error": e.to_string(),
                        "tokens": tokens,
                    }).to_string())
                }
                ryeos_runtime::ResolveError::VerbNotFound { .. } => {
                    RouteDispatchError::Internal(e.to_string())
                }
                ryeos_runtime::ResolveError::VerbNotExecutable { .. } => {
                    RouteDispatchError::BadRequest(e.to_string())
                }
            })?;

            tracing::debug!(
                tokens = ?tokens,
                verb = %resolved.verb,
                consumed = resolved.consumed,
                item_ref = %resolved.execute_ref,
                deprecated = resolved.deprecated,
                "resolved tokens via shared resolver"
            );

            if resolved.deprecated {
                tracing::warn!(
                    verb = %resolved.verb,
                    replacement = ?resolved.replacement_tokens,
                    removed_in = ?resolved.removed_in,
                    "deprecated alias used"
                );
            }

            request.item_ref = Some(resolved.execute_ref);
            request.parameters = resolved.parameters;
        }

        let item_ref = request.item_ref.as_ref().unwrap();

        let site_id = state.threads.site_id();
        let project_source = request.project_source.clone().unwrap_or_default();
        let project_path = match &request.project_path {
            Some(p) => project_source::normalize_project_path(p),
            None => {
                if matches!(project_source, ProjectSource::PushedHead) {
                    return Ok((
                        StatusCode::BAD_REQUEST,
                        axum::Json(json!({ "error": "project_path is required when project_source is pushed_head" })),
                    ).into_response());
                }
                std::env::current_dir()
                    .map_err(|err| RouteDispatchError::Internal(err.to_string()))?
            }
        };

        // Reject validate_only + pushed_head.
        if request.validate_only && matches!(project_source, ProjectSource::PushedHead) {
            return Ok((
                StatusCode::BAD_REQUEST,
                axum::Json(json!({ "error": "validate_only is not supported with pushed_head project_source" })),
            ).into_response());
        }

        // Resolve project execution context.
        let checkout_id = format!("pre-{}-{:08x}", lillux::time::timestamp_millis(), rand::random::<u32>());
        let mut project_ctx = match project_source::resolve_project_context(
            &state,
            &project_source,
            &project_path,
            &caller_principal_id,
            &checkout_id,
        ) {
            Ok(ctx) => ctx,
            Err(err) => {
                use crate::dispatch_error::DispatchError;
                use crate::execution::project_source::ProjectSourceError as PSE;
                let dispatch_err: DispatchError = match err {
                    err @ PSE::PushFirst { .. } => {
                        DispatchError::ProjectSourcePushFirst(err.to_string())
                    }
                    PSE::CheckoutFailed(detail) => {
                        DispatchError::ProjectSourceCheckoutFailed(detail)
                    }
                    PSE::Other(detail) => DispatchError::ProjectSource(detail),
                };
                return Ok(dispatch_error_response(dispatch_err));
            }
        };

        let temp_dir_guard = TempDirGuard::new(project_ctx.temp_dir.take());

        // Build plan context.
        use ryeos_engine::contracts::{EffectivePrincipal, PlanContext, ProjectContext};

        let plan_ctx = PlanContext {
            requested_by: EffectivePrincipal::Local(
                ryeos_engine::contracts::Principal {
                    fingerprint: caller_principal_id.clone(),
                    scopes: caller_scopes.clone(),
                },
            ),
            project_context: ProjectContext::LocalPath {
                path: project_ctx.effective_path.clone(),
            },
            current_site_id: site_id.to_string(),
            origin_site_id: site_id.to_string(),
            execution_hints: Default::default(),
            validate_only: request.validate_only,
        };

        let exec_ctx = crate::service_executor::ExecutionContext {
            principal_fingerprint: caller_principal_id.clone(),
            caller_scopes: caller_scopes.clone(),
            engine: state.engine.clone(),
            plan_ctx,
            requested_op: request.operation.clone(),
            requested_inputs: request.inputs.clone(),
        };

        // Parse the user-supplied root ref.
        let root_canonical = match ryeos_engine::canonical_ref::CanonicalRef::parse(item_ref) {
            Ok(c) => c,
            Err(e) => {
                return Ok((
                    StatusCode::BAD_REQUEST,
                    axum::Json(json!({
                        "error": format!("invalid item ref '{}': {e}", item_ref)
                    })),
                ).into_response());
            }
        };

        let dispatch_req = crate::dispatch::DispatchRequest {
            launch_mode: request.launch_mode.as_str(),
            target_site_id: request.target_site_id.as_deref(),
            project_source_is_pushed_head: matches!(project_source, ProjectSource::PushedHead),
            validate_only: request.validate_only,
            params: request.parameters.clone(),
            acting_principal: caller_principal_id.as_str(),
            project_path: &project_ctx.effective_path,
            original_project_path: project_ctx.original_path.clone(),
            snapshot_hash: project_ctx.snapshot_hash.clone(),
            temp_dir: temp_dir_guard.disarm(),
            original_root_kind: root_canonical.kind.as_str(),
            pre_minted_thread_id: None,
            operation: request.operation.clone(),
            inputs: request.inputs.clone(),
        };

        match crate::dispatch::dispatch(item_ref, &dispatch_req, &exec_ctx, &state).await {
            Ok(value) => Ok(axum::Json(value).into_response()),
            Err(e) => {
                let status = e.http_status();
                let payload = crate::structured_error::StructuredErrorPayload::from(&e);
                Ok((status, axum::Json(payload.to_value())).into_response())
            }
        }
    }
}

/// Map a `DispatchError` into an HTTP response with the correct status code
/// and structured error payload.
fn dispatch_error_response(e: crate::dispatch_error::DispatchError) -> axum::response::Response {
    let status = e.http_status();
    let payload = crate::structured_error::StructuredErrorPayload::from(&e);
    (status, axum::Json(payload.to_value())).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::raw::{RawLimits, RawRequest, RawResponseSpec};

    fn make_raw(auth: &str, body: RawRequestBody) -> RawRouteSpec {
        RawRouteSpec {
            section: "routes".into(),
            category: None,
            id: "core/execute".into(),
            path: "/execute".into(),
            methods: ["POST".into()].into_iter().collect(),
            auth: auth.into(),
            auth_config: None,
            limits: RawLimits::default(),
            response: RawResponseSpec {
                mode: "execute".into(),
                source: None,
                source_config: serde_json::Value::Null,
                status: None,
                content_type: None,
                body_b64: None,
            },
            execute: None,
            request: RawRequest { body },
            source_file: std::path::PathBuf::from("/test/execute.yaml"),
        }
    }

    #[test]
    fn compile_succeeds_on_valid_route() {
        let mode = ExecuteMode;
        let raw = make_raw("ryeos_signed", RawRequestBody::Json);
        let result = mode.compile(&raw);
        assert!(result.is_ok(), "expected Ok, got: {:?}", result.err());
    }

    #[test]
    fn compile_rejects_non_ryeos_signed_auth() {
        let mode = ExecuteMode;
        let raw = make_raw("none", RawRequestBody::Json);
        let err = match mode.compile(&raw) {
            Err(e) => e,
            Ok(_) => panic!("expected error"),
        };
        let msg = format!("{err}");
        assert!(msg.contains("requires auth = 'ryeos_signed'"), "got: {msg}");
    }

    #[test]
    fn compile_rejects_body_none() {
        let mode = ExecuteMode;
        let raw = make_raw("ryeos_signed", RawRequestBody::None);
        let err = match mode.compile(&raw) {
            Err(e) => e,
            Ok(_) => panic!("expected error"),
        };
        let msg = format!("{err}");
        assert!(msg.contains("request.body = json"), "got: {msg}");
    }

    #[test]
    fn compile_rejects_response_source() {
        let mode = ExecuteMode;
        let mut raw = make_raw("ryeos_signed", RawRequestBody::Json);
        raw.response.source = Some("service:x".into());
        let err = match mode.compile(&raw) {
            Err(e) => e,
            Ok(_) => panic!("expected error"),
        };
        let msg = format!("{err}");
        assert!(msg.contains("must not declare response.source"), "got: {msg}");
    }

    #[test]
    fn compile_rejects_execute_block() {
        use crate::routes::raw::RawExecute;
        let mode = ExecuteMode;
        let mut raw = make_raw("ryeos_signed", RawRequestBody::Json);
        raw.execute = Some(RawExecute {
            item_ref: "tool:x/y".into(),
            params: serde_json::Value::Null,
        });
        let err = match mode.compile(&raw) {
            Err(e) => e,
            Ok(_) => panic!("expected error"),
        };
        let msg = format!("{err}");
        assert!(msg.contains("must not have a top-level 'execute' block"), "got: {msg}");
    }

    #[test]
    fn compile_rejects_static_mode_fields() {
        let mode = ExecuteMode;
        let mut raw = make_raw("ryeos_signed", RawRequestBody::Json);
        raw.response.status = Some(200);
        let err = match mode.compile(&raw) {
            Err(e) => e,
            Ok(_) => panic!("expected error"),
        };
        let msg = format!("{err}");
        assert!(msg.contains("must not set static-mode fields"), "got: {msg}");
    }
}
