//! Generic compiled invoker for service canonical refs.
//!
//! A single `CompiledServiceInvocation` wraps any registered service handler
//! (identified by endpoint string). No per-service hand-written invoker
//! structs — the 14+ handlers all go through this one adapter.

use std::sync::Arc;

use ryeos_runtime::authorizer::{Authorizer, AuthorizationPolicy};

use crate::dispatch_error::RouteDispatchError;
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
            let authorizer = Authorizer::new(ctx.state.verb_registry.clone());
            authorizer
                .authorize(&principal.scopes, &policy)
                .map_err(|_| RouteDispatchError::Unauthorized)?;
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

        // Call the service handler in-process.
        let state_arc = Arc::new(ctx.state);
        let result = handler(ctx.input, state_arc)
            .await
            .map_err(|e| RouteDispatchError::Internal(format!("service error: {e}")))?;

        Ok(RouteInvocationResult::Json(result))
    }
}
