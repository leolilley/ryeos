//! Auth invoker for the `none` verifier.
//!
//! Produces an anonymous principal with no scopes. Used for public routes.

use std::collections::BTreeMap;

use crate::dispatch_error::RouteDispatchError;
use crate::routes::invocation::{
    CompiledRouteInvocation, PrincipalPolicy, RouteInvocationContract, RouteInvocationContext,
    RouteInvocationOutput, RouteInvocationResult, RoutePrincipal,
};

pub struct CompiledNoneVerifier;

static NONE_CONTRACT: RouteInvocationContract = RouteInvocationContract {
    output: RouteInvocationOutput::Principal,
    principal: PrincipalPolicy::Forbidden,
};

#[axum::async_trait]
impl CompiledRouteInvocation for CompiledNoneVerifier {
    fn contract(&self) -> &'static RouteInvocationContract {
        &NONE_CONTRACT
    }

    async fn invoke(
        &self,
        ctx: RouteInvocationContext,
    ) -> Result<RouteInvocationResult, RouteDispatchError> {
        Ok(RouteInvocationResult::Principal(RoutePrincipal {
            id: format!("route:{}", ctx.route_id),
            scopes: vec![],
            verifier_key: "none",
            verified: false,
            metadata: BTreeMap::new(),
        }))
    }
}
