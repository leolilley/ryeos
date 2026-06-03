//! `browser_session` auth invoker — validates the `ryeos_session` cookie.
//!
//! When a route declares `auth: browser_session`, this verifier extracts the
//! `ryeos_session` cookie from the request, looks it up in the
//! `BrowserSessionStore`, and produces a principal with the session's
//! granted capabilities.

use std::collections::BTreeMap;
use std::sync::Arc;

use ryeos_api::route_error::{RouteConfigError, RouteDispatchError};
use ryeos_api::routes::invocation::{
    CompiledRouteInvocation, PrincipalPolicy, RouteInvocationContext, RouteInvocationContract,
    RouteInvocationOutput, RouteInvocationResult, RoutePrincipal,
};
use ryeos_api::routes::invokers::AuthVerifierFactory;

use crate::state::UiState;

pub struct CompiledBrowserSessionVerifier {
    pub ui: Arc<UiState>,
}

/// Factory that creates `CompiledBrowserSessionVerifier` instances.
///
/// Registered as `"browser_session"` in the auth verifier registry by
/// the UI composition root.
pub struct BrowserSessionAuthFactory {
    pub ui: Arc<UiState>,
}

impl AuthVerifierFactory for BrowserSessionAuthFactory {
    fn compile(
        &self,
        _auth_config: Option<&serde_json::Value>,
        _route_id: &str,
    ) -> Result<Arc<dyn CompiledRouteInvocation>, RouteConfigError> {
        Ok(Arc::new(CompiledBrowserSessionVerifier {
            ui: self.ui.clone(),
        }))
    }
}

static BROWSER_SESSION_CONTRACT: RouteInvocationContract = RouteInvocationContract {
    output: RouteInvocationOutput::Principal,
    principal: PrincipalPolicy::Forbidden,
};

#[axum::async_trait]
impl CompiledRouteInvocation for CompiledBrowserSessionVerifier {
    fn contract(&self) -> &'static RouteInvocationContract {
        &BROWSER_SESSION_CONTRACT
    }

    async fn invoke(
        &self,
        ctx: RouteInvocationContext,
    ) -> Result<RouteInvocationResult, RouteDispatchError> {
        let session_id =
            extract_session_cookie(&ctx.headers).ok_or_else(|| RouteDispatchError::Unauthorized)?;

        let session = self
            .ui
            .browser_sessions
            .get_session(&session_id)
            .ok_or(RouteDispatchError::Unauthorized)?;

        Ok(RouteInvocationResult::Principal(RoutePrincipal {
            id: format!("session:{}", session.session_id),
            scopes: session.granted_caps,
            verifier_key: "browser_session",
            verified: false,
            metadata: {
                let mut m = BTreeMap::new();
                if let Some(ref root) = session.project_root {
                    m.insert("project_root".into(), root.clone());
                }
                if let Some(ref principal_id) = session.user_principal_id {
                    m.insert("user_principal_id".into(), principal_id.clone());
                }
                m
            },
        }))
    }
}

/// Extract the `ryeos_session` cookie value from request headers.
fn extract_session_cookie(headers: &axum::http::HeaderMap) -> Option<String> {
    extract_session_cookie_impl(headers)
}

/// Visible for testing.
pub fn extract_session_cookie_for_test(headers: &axum::http::HeaderMap) -> Option<String> {
    extract_session_cookie_impl(headers)
}

fn extract_session_cookie_impl(headers: &axum::http::HeaderMap) -> Option<String> {
    let cookie_header = headers.get("cookie")?.to_str().ok()?;
    for cookie in cookie_header.split(';') {
        let trimmed = cookie.trim();
        if let Some(value) = trimmed.strip_prefix("ryeos_session=") {
            return Some(value.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::header::COOKIE;
    use axum::http::HeaderMap;

    #[test]
    fn extract_session_cookie_found() {
        let mut headers = HeaderMap::new();
        headers.insert(
            COOKIE,
            "other=val; ryeos_session=abc-123; foo=bar".parse().unwrap(),
        );
        assert_eq!(
            extract_session_cookie(&headers),
            Some("abc-123".to_string())
        );
    }

    #[test]
    fn extract_session_cookie_first() {
        let mut headers = HeaderMap::new();
        headers.insert(COOKIE, "ryeos_session=first".parse().unwrap());
        assert_eq!(extract_session_cookie(&headers), Some("first".to_string()));
    }

    #[test]
    fn extract_session_cookie_missing() {
        let mut headers = HeaderMap::new();
        headers.insert(COOKIE, "other=val".parse().unwrap());
        assert_eq!(extract_session_cookie(&headers), None);
    }

    #[test]
    fn extract_session_cookie_no_cookie_header() {
        let headers = HeaderMap::new();
        assert_eq!(extract_session_cookie(&headers), None);
    }
}
