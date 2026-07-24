//! `browser_launch` response mode — consume a browser launch token, set the
//! session cookie, and redirect to the browser shell.
//!
//! This is intentionally separate from `json`: `service:ui/launch` returns a
//! structured envelope so tests and service callers can inspect it, while the
//! HTTP browser route must translate that envelope into `Set-Cookie` + redirect.

use std::sync::Arc;

use axum::http::{header, HeaderValue, StatusCode};
use axum::response::IntoResponse;

use crate::route_error::{RouteConfigError, RouteDispatchError};
use crate::routes::compile::{
    CompiledResponseMode, CompiledRoute, ResponseMode, RouteDispatchContext,
};
use crate::routes::interpolation;
use crate::routes::invocation::{
    CompiledRouteInvocation, InvocationCheck, RouteInvocationContext, RouteInvocationOutput,
    RouteInvocationResult,
};
use ryeos_app::route_raw::RawRouteSpec;

pub struct BrowserLaunchMode {
    pub service_descriptors: &'static [crate::registry::ServiceDescriptor],
}

pub struct CompiledBrowserLaunchMode {
    invoker: Arc<dyn CompiledRouteInvocation>,
    source_config_template: serde_json::Value,
}

impl ResponseMode for BrowserLaunchMode {
    fn key(&self) -> &'static str {
        "browser_launch"
    }

    fn compile(
        &self,
        raw: &RawRouteSpec,
    ) -> Result<Arc<dyn CompiledResponseMode>, RouteConfigError> {
        if raw.execute.is_some() {
            return Err(RouteConfigError::InvalidResponseSpec {
                id: raw.id.clone(),
                mode: self.key().into(),
                reason: "browser_launch mode must not have a top-level 'execute' block".into(),
            });
        }
        if raw.response.status.is_some()
            || raw.response.content_type.is_some()
            || raw.response.body_b64.is_some()
        {
            return Err(RouteConfigError::InvalidResponseSpec {
                id: raw.id.clone(),
                mode: self.key().into(),
                reason: "browser_launch mode must not set static-mode fields \
                    (status / content_type / body_b64)"
                    .into(),
            });
        }

        let source_str = raw.response.source.as_deref().ok_or_else(|| {
            RouteConfigError::InvalidSourceConfig {
                id: raw.id.clone(),
                src: self.key().into(),
                reason: "browser_launch mode requires `response.source`".into(),
            }
        })?;

        let invoker = crate::routes::invokers::compile_canonical_ref_invoker_with_descriptors(
            source_str,
            &raw.id,
            self.service_descriptors,
        )?;

        if !raw.response.source_config.is_null() {
            let declared = extract_path_captures(&raw.path);
            interpolation::validate_path_templates(
                &raw.response.source_config,
                &declared,
                &raw.id,
                self.key(),
            )?;
        }

        Ok(Arc::new(CompiledBrowserLaunchMode {
            invoker,
            source_config_template: raw.response.source_config.clone(),
        }))
    }
}

#[axum::async_trait]
impl CompiledResponseMode for CompiledBrowserLaunchMode {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn handle(
        &self,
        compiled: &CompiledRoute,
        ctx: RouteDispatchContext,
    ) -> Result<axum::response::Response, RouteDispatchError> {
        let input = if !self.source_config_template.is_null() {
            interpolation::interpolate_path(&self.source_config_template, &ctx.captures)?
        } else {
            serde_json::Value::Object(serde_json::Map::new())
        };

        let inv_ctx = RouteInvocationContext {
            route_id: compiled.id.clone().into(),
            method: ctx.request_parts.method,
            uri: ctx.request_parts.uri,
            captures: std::collections::BTreeMap::from_iter(ctx.captures),
            headers: ctx.request_parts.headers,
            body_raw: ctx.body_raw,
            input,
            principal: Some(ctx.principal),
            workspace_lifeline: None,
            launch_timings: None,
            state: ctx.state,
            webhook_dedupe: ctx.webhook_dedupe,
        };

        let result = crate::routes::invocation::invoke_checked(
            self.invoker.as_ref(),
            InvocationCheck {
                expected_output: RouteInvocationOutput::Json,
            },
            inv_ctx,
        )
        .await?;

        let RouteInvocationResult::Json(value) = result else {
            unreachable!("invoke_checked enforces Json")
        };

        let redirect = value
            .get("redirect")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                RouteDispatchError::Internal("browser_launch response missing redirect".to_string())
            })?;
        let cookie = value.get("cookie").ok_or_else(|| {
            RouteDispatchError::Internal("browser_launch response missing cookie".to_string())
        })?;
        let cookie_header = cookie_header(cookie)?;

        let mut response = StatusCode::SEE_OTHER.into_response();
        let headers = response.headers_mut();
        headers.insert(
            header::LOCATION,
            HeaderValue::from_str(redirect).map_err(|e| {
                RouteDispatchError::Internal(format!("invalid browser_launch redirect: {e}"))
            })?,
        );
        headers.insert(
            header::SET_COOKIE,
            HeaderValue::from_str(&cookie_header).map_err(|e| {
                RouteDispatchError::Internal(format!("invalid browser_launch cookie: {e}"))
            })?,
        );
        Ok(response)
    }
}

fn cookie_header(cookie: &serde_json::Value) -> Result<String, RouteDispatchError> {
    let name = cookie.get("name").and_then(|v| v.as_str()).ok_or_else(|| {
        RouteDispatchError::Internal("browser_launch cookie missing name".to_string())
    })?;
    let value = cookie
        .get("value")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            RouteDispatchError::Internal("browser_launch cookie missing value".to_string())
        })?;
    let path = cookie.get("path").and_then(|v| v.as_str()).unwrap_or("/");
    let same_site = cookie
        .get("same_site")
        .and_then(|v| v.as_str())
        .unwrap_or("Lax");
    let http_only = cookie
        .get("http_only")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let secure = cookie
        .get("secure")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let mut header = format!("{name}={value}; Path={path}; SameSite={same_site}");
    if http_only {
        header.push_str("; HttpOnly");
    }
    if secure {
        header.push_str("; Secure");
    }
    Ok(header)
}

fn extract_path_captures(path: &str) -> Vec<String> {
    let mut captures = Vec::new();
    let mut chars = path.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '{' {
            continue;
        }
        let mut name = String::new();
        while let Some(&next) = chars.peek() {
            chars.next();
            if next == '}' {
                break;
            }
            name.push(next);
        }
        if !name.is_empty() {
            captures.push(name);
        }
    }
    captures
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookie_header_formats_browser_session_cookie() {
        // Mirrors the production envelope from `service:ui/launch`,
        // which sets SameSite=Strict.
        let header = cookie_header(&serde_json::json!({
            "name": "ryeos_session",
            "value": "abc",
            "path": "/ui",
            "same_site": "Strict",
            "http_only": true,
            "secure": true,
        }))
        .unwrap();

        assert_eq!(
            header,
            "ryeos_session=abc; Path=/ui; SameSite=Strict; HttpOnly; Secure"
        );
    }
}
