use std::sync::Arc;

use axum::http::Uri;
use serde_json::Value;

use crate::auth;
use crate::dispatch_error::{RouteConfigError, RouteDispatchError};
use crate::routes::compile::{
    AuthVerifier, CompiledAuthVerifier, RoutePrincipal, VerifierRequestContext,
};

pub struct RyeSignedVerifier;

impl AuthVerifier for RyeSignedVerifier {
    fn key(&self) -> &'static str {
        "rye_signed"
    }

    fn validate_route_config(
        &self,
        _auth_config: Option<&Value>,
    ) -> Result<Arc<dyn CompiledAuthVerifier>, RouteConfigError> {
        Ok(Arc::new(CompiledRyeSignedVerifier))
    }
}

struct CompiledRyeSignedVerifier;

impl CompiledAuthVerifier for CompiledRyeSignedVerifier {
    fn verify(
        &self,
        route_id: &str,
        req: &VerifierRequestContext,
        state: &crate::state::AppState,
    ) -> Result<RoutePrincipal, RouteDispatchError> {
        let method_str = req.method.as_str();
        let path = req.path;
        let uri: Uri = path
            .parse()
            .map_err(|e| {
                RouteDispatchError::BadRequest(format!("invalid path '{}': {}", path, e))
            })?;

        let principal =
            auth::verify_request(state, method_str, &uri, req.headers, req.body_raw)
                .map_err(|msg| {
                    tracing::warn!(
                        route_id = route_id,
                        error = %msg,
                        "rye_signed verification failed"
                    );
                    RouteDispatchError::Unauthorized
                })?;

        Ok(RoutePrincipal {
            id: format!("fp:{}", principal.fingerprint),
            scopes: principal.scopes,
            verifier_key: "rye_signed",
            verified: true,
        })
    }
}
