//! Auth verifier for the one-use external supervisor bootstrap capability.

use std::collections::BTreeMap;

use crate::route_error::{RouteConfigError, RouteDispatchError};
use crate::routes::invocation::{
    CompiledRouteInvocation, PrincipalPolicy, RouteInvocationContext, RouteInvocationContract,
    RouteInvocationOutput, RouteInvocationResult, RoutePrincipal,
};
use crate::routes::invokers::AuthVerifierFactory;

pub struct ExternalOccurrenceAuthFactory;

impl AuthVerifierFactory for ExternalOccurrenceAuthFactory {
    fn compile(
        &self,
        auth_config: Option<&serde_json::Value>,
        route_id: &str,
    ) -> Result<std::sync::Arc<dyn CompiledRouteInvocation>, RouteConfigError> {
        if auth_config.is_some_and(|value| !value.is_null()) {
            return Err(RouteConfigError::InvalidSourceConfig {
                id: route_id.into(),
                src: "external_occurrence_verifier".into(),
                reason: "auth_config is not accepted".into(),
            });
        }
        Ok(std::sync::Arc::new(CompiledExternalOccurrenceVerifier))
    }
}

struct CompiledExternalOccurrenceVerifier;

static CONTRACT: RouteInvocationContract = RouteInvocationContract {
    output: RouteInvocationOutput::Principal,
    principal: PrincipalPolicy::Forbidden,
};

#[axum::async_trait]
impl CompiledRouteInvocation for CompiledExternalOccurrenceVerifier {
    fn contract(&self) -> &'static RouteInvocationContract {
        &CONTRACT
    }

    async fn invoke(
        &self,
        ctx: RouteInvocationContext,
    ) -> Result<RouteInvocationResult, RouteDispatchError> {
        let request: crate::handlers::external_execution_channel::Request =
            serde_json::from_slice(&ctx.body_raw).map_err(|_| RouteDispatchError::Unauthorized)?;
        request
            .validate_shape()
            .map_err(|_| RouteDispatchError::Unauthorized)?;
        let authenticated = ryeos_app::external_placement::authenticate_external_channel_bootstrap(
            &ctx.state,
            &request.placement_thread_id,
            &request.occurrence_id,
            &request.bootstrap_capability,
        )
        .map_err(|error| {
            tracing::warn!(
                route_id = %ctx.route_id,
                placement = %request.placement_thread_id,
                error = %error,
                "external occurrence verification failed"
            );
            RouteDispatchError::Unauthorized
        })?;
        let mut metadata = BTreeMap::new();
        metadata.insert(
            "placement_thread_id".into(),
            authenticated.placement_thread_id().to_owned(),
        );
        metadata.insert(
            "allocation_request_digest".into(),
            authenticated.allocation_request_digest().to_owned(),
        );
        Ok(RouteInvocationResult::Principal(RoutePrincipal {
            id: request.principal_id(),
            scopes: Vec::new(),
            verifier_key: "external_occurrence",
            verified: true,
            authorized_key_class: None,
            authenticated_origin_site_id: None,
            authenticated_grant_authority: None,
            metadata,
        }))
    }
}
