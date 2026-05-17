//! Generic compiled invoker for service canonical refs.
//!
//! A single `CompiledServiceInvocation` wraps any registered service handler
//! (identified by endpoint string). No per-service hand-written invoker
//! structs — the 14+ handlers all go through this one adapter.

use std::sync::Arc;

use ryeos_runtime::authorizer::AuthorizationPolicy;

use crate::route_error::RouteDispatchError;
use crate::routes::invocation::{
    CompiledRouteInvocation, PrincipalPolicy, RouteInvocationContract, RouteInvocationContext,
    RouteInvocationOutput, RouteInvocationResult,
};

/// Generic invoker for `service:` canonical refs.
///
/// At compile time the endpoint is validated against the service descriptor
/// list. At runtime the endpoint is looked up in the `ServiceRegistry` and
/// called with the interpolated input.
pub struct CompiledServiceInvocation {
    /// Service endpoint string (e.g., `"threads.get"`).
    pub endpoint: String,
    /// Capability scopes required by the service (from descriptor).
    pub required_caps: Vec<String>,
}

static SERVICE_CONTRACT: RouteInvocationContract = RouteInvocationContract {
    output: RouteInvocationOutput::Json,
    principal: PrincipalPolicy::Optional,
};

#[axum::async_trait]
impl CompiledRouteInvocation for CompiledServiceInvocation {
    fn contract(&self) -> &'static RouteInvocationContract {
        &SERVICE_CONTRACT
    }

    async fn invoke(
        &self,
        ctx: RouteInvocationContext,
    ) -> Result<RouteInvocationResult, RouteDispatchError> {
        // Cap enforcement: check required_caps against principal scopes
        // using the unified Authorizer (wildcards, implication expansion).
        if !self.required_caps.is_empty() {
            let principal = ctx
                .principal
                .as_ref()
                .ok_or(RouteDispatchError::Unauthorized)?;
            let policy = AuthorizationPolicy::require_all(
                &self.required_caps.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
            );
            ctx.state
                .authorizer
                .authorize(&principal.scopes, &policy)
                .map_err(|_| {
                    RouteDispatchError::Forbidden(format!(
                        "missing required capability: {}",
                        self.required_caps.join(", ")
                    ))
                })?;
        }

        // Look up the handler by endpoint.
        let handler = ctx
            .state
            .services
            .get(&self.endpoint)
            .cloned()
            .ok_or_else(|| {
                RouteDispatchError::Internal(format!(
                    "service endpoint '{}' not found in registry",
                    self.endpoint
                ))
            })?;

        // Inject typed handler context for principal-aware handlers.
        // Handlers opt in by including `_ctx: HandlerContext` (with
        // `#[serde(default)]`) in their Request struct.
        // Only inject with verified=true when the principal was
        // cryptographically authenticated. Anonymous routes produce
        // a principal with verified=false — we still inject the
        // context but handlers see verified=false and must not trust
        // the fingerprint for ownership decisions.
        let mut input = ctx.input;
        if let Some(ref principal) = ctx.principal {
            if let serde_json::Value::Object(ref mut map) = input {
                map.insert(
                    "_ctx".into(),
                    serde_json::json!({
                        "fingerprint": principal.id,
                        "scopes": principal.scopes,
                        "verified": principal.verified,
                    }),
                );
            }
        }

        // Call the service handler in-process.
        let state_arc = Arc::new(ctx.state);
        let result = handler(input, state_arc)
            .await
            .map_err(|e| {
                // Try to extract a typed HandlerError and map it to
                // the appropriate RouteDispatchError variant. Generic
                // errors become 500.
                if let Some(he) = crate::handler_error::extract_handler_error(&e) {
                    match he {
                        crate::handler_error::HandlerError::NotFound => {
                            RouteDispatchError::NotFound
                        }
                        crate::handler_error::HandlerError::Forbidden(msg) => {
                            RouteDispatchError::Forbidden(msg)
                        }
                        crate::handler_error::HandlerError::BadRequest(msg) => {
                            RouteDispatchError::BadRequest(msg)
                        }
                        crate::handler_error::HandlerError::Internal(msg) => {
                            RouteDispatchError::Internal(msg)
                        }
                    }
                } else {
                    RouteDispatchError::Internal(format!("service error: {e}"))
                }
            })?;

        Ok(RouteInvocationResult::Json(result))
    }
}
