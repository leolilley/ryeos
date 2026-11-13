//! Auth invoker for the `ryeos_signed` verifier.
//!
//! Delegates to `crate::auth::verify_request` for cryptographic verification
//! of Ed25519-signed requests.

use std::collections::BTreeMap;

use crate::route_error::RouteDispatchError;
use crate::routes::invocation::{
    CompiledRouteInvocation, PrincipalPolicy, RouteInvocationContext, RouteInvocationContract,
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
        .map_err(|error| {
            tracing::warn!(
                route_id = %ctx.route_id,
                error = %error,
                "ryeos_signed verification failed"
            );
            match error {
                // The caller proved it holds the key: tell it why the grant is
                // refused and how to cure that.
                crate::auth::RequestAuthError::Grant(refusal) => RouteDispatchError::Structured {
                    code: refusal.code().to_owned(),
                    status: 401,
                    body: serde_json::json!({
                        "error": refusal.to_string(),
                        "error_code": refusal.code(),
                    }),
                    thread_id: None,
                },
                crate::auth::RequestAuthError::Refused(_) => RouteDispatchError::Unauthorized,
            }
        })?;

        Ok(RouteInvocationResult::Principal(RoutePrincipal {
            id: format!("fp:{}", principal.fingerprint),
            scopes: principal.scopes,
            verifier_key: "ryeos_signed",
            verified: true,
            authorized_key_class: Some(principal.principal_class),
            authenticated_origin_site_id: principal.authenticated_site_id,
            authenticated_grant_authority: Some(principal.grant_authority),
            metadata: BTreeMap::from([(
                crate::routes::invocation::SIGNED_REQUEST_DIGEST_METADATA_KEY.to_owned(),
                principal.request_digest,
            )]),
        }))
    }
}
