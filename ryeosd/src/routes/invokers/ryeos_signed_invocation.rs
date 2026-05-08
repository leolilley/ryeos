//! Auth invoker for the `ryeos_signed` verifier.
//!
//! Delegates to `crate::auth::verify_request` for cryptographic verification
//! of Ed25519-signed requests.

use std::collections::BTreeMap;

use crate::dispatch_error::RouteDispatchError;
use crate::routes::invocation::{
    CompiledRouteInvocation, PrincipalPolicy, RouteInvocationContract, RouteInvocationContext,
    RouteInvocationOutput, RouteInvocationResult, RoutePrincipal,
};

pub struct CompiledRyeosSignedVerifier;

static RYEOS_SIGNED_CONTRACT: RouteInvocationContract = RouteInvocationContract {
    output: RouteInvocationOutput::Principal,
    principal: PrincipalPolicy::Forbidden,
};

#[axum::async_trait]
impl CompiledRouteInvocation for CompiledRyeosSignedVerifier {
    fn contract(&self) -> &'static RouteInvocationContract {
        &RYEOS_SIGNED_CONTRACT
    }

    async fn invoke(
        &self,
        ctx: RouteInvocationContext,
    ) -> Result<RouteInvocationResult, RouteDispatchError> {
        let method_str = ctx.method.as_str();

        let principal = crate::auth::verify_request(
            &ctx.state,
            method_str,
            &ctx.uri,
            &ctx.headers,
            &ctx.body_raw,
        )
        .map_err(|msg| {
            tracing::warn!(
                route_id = %ctx.route_id,
                error = %msg,
                "ryeos_signed verification failed"
            );
            RouteDispatchError::Unauthorized
        })?;

        Ok(RouteInvocationResult::Principal(RoutePrincipal {
            id: format!("fp:{}", principal.fingerprint),
            scopes: principal.scopes,
            verifier_key: "ryeos_signed",
            verified: true,
            metadata: BTreeMap::new(),
        }))
    }
}
