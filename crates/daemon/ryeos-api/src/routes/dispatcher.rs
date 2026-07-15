use std::collections::BTreeMap;
use std::time::Duration;

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use crate::api_state::ApiState;
use crate::routes::compile::RouteDispatchContext;
use crate::routes::invocation::{
    InvocationCheck, RouteInvocationContext, RouteInvocationOutput, RouteInvocationResult,
};
use crate::routes::limits::RouteLimiter;

pub async fn route_dispatcher(State(api_state): State<ApiState>, request: Request) -> Response {
    let table = api_state.route_table.load_full();
    let app_state = (*api_state.app).clone();
    let webhook_dedupe = api_state.webhook_dedupe.clone();

    let method = request.method().clone();
    let path = request.uri().path().to_string();

    let (route, captures) = match table.match_request(&method, &path) {
        Some(r) => r,
        None => {
            // Distinguish "path exists but not for this method" (405) from "path
            // matches no route" (404). A POST-only route hit with GET used to
            // return 404, which reads as "route missing" and sent operators
            // chasing phantom deploy/version problems — 405 + Allow makes it
            // obvious the route is there.
            let allowed = table.allowed_methods_for_path(&path);
            if !allowed.is_empty() {
                let allow = allowed
                    .iter()
                    .map(|m| m.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                return (
                    StatusCode::METHOD_NOT_ALLOWED,
                    [(axum::http::header::ALLOW, allow)],
                    axum::Json(serde_json::json!({
                        "error": "method not allowed",
                        "path": path,
                        "method": method.as_str(),
                        "allowed_methods": allowed.iter().map(|m| m.as_str()).collect::<Vec<_>>(),
                    })),
                )
                    .into_response();
            }
            // True dispatcher-level 404: no route matched this path for any
            // method. The body is identical to the json-mode null/404 and the
            // typed NotFound, so log here to disambiguate "route not loaded /
            // wrong host" from "handler said not-found" in operator triage.
            // debug!, not warn!: internet-facing nodes are scanned constantly,
            // so unmatched paths are routine background noise, not incidents.
            tracing::debug!(
                method = %method,
                path = %path,
                "no route matched request path; returning HTTP 404"
            );
            return (
                StatusCode::NOT_FOUND,
                axum::Json(serde_json::json!({
                    "error": "not found",
                })),
            )
                .into_response();
        }
    };

    let _permit = match route.semaphore.clone().try_acquire_owned() {
        Ok(p) => p,
        Err(tokio::sync::TryAcquireError::NoPermits) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                axum::Json(serde_json::json!({"error": "too many concurrent requests"})),
            )
                .into_response();
        }
        Err(tokio::sync::TryAcquireError::Closed) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(serde_json::json!({"error": "route semaphore closed"})),
            )
                .into_response();
        }
    };

    let limiter = RouteLimiter::from_limits(&route.limits);

    if let Err(resp) = limiter.check_content_length(request.headers()) {
        return resp;
    }

    let (parts, body) = request.into_parts();

    let body_bytes = match limiter.read_bounded_body(body).await {
        Ok(b) => b,
        Err(resp) => return resp,
    };
    let body_raw = body_bytes.to_vec();

    // Build invocation context for auth.
    let auth_ctx = RouteInvocationContext {
        route_id: route.id.clone().into(),
        method: method.clone(),
        uri: parts.uri.clone(),
        captures: BTreeMap::from_iter(captures.clone()),
        headers: parts.headers.clone(),
        body_raw: body_raw.clone(),
        input: serde_json::Value::Null,
        principal: None,
        workspace_lifeline: None,
        state: app_state.clone(),
        webhook_dedupe: webhook_dedupe.clone(),
    };

    // Invoke auth invoker through the central contract enforcement layer.
    let auth_result = match crate::routes::invocation::invoke_checked(
        route.auth_invoker.as_ref(),
        InvocationCheck {
            expected_output: RouteInvocationOutput::Principal,
        },
        auth_ctx,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => return e.into_response(),
    };

    let principal = match auth_result {
        RouteInvocationResult::Principal(p) => p,
        // invoke_checked guarantees Principal; any other variant is already
        // caught as an Internal error by the enforcement layer.
        _ => unreachable!("invoke_checked enforces Principal for auth"),
    };

    let route_dispatch_ctx = RouteDispatchContext {
        captures,
        request_parts: parts,
        body_raw,
        principal,
        state: app_state,
        webhook_dedupe,
    };

    let route_ref = route.clone();

    let is_streaming = route_ref.response_mode.is_streaming();
    let no_timeout = is_streaming && limiter.timeout == Duration::ZERO;

    if no_timeout {
        match route_ref
            .response_mode
            .handle(&route_ref, route_dispatch_ctx)
            .await
        {
            Ok(resp) => resp,
            Err(e) => e.into_response(),
        }
    } else {
        // The timeout bounds the CLIENT's wait, never the work: the handler
        // runs in its own task, so hitting the route timeout (or the client
        // disconnecting) abandons only the response. Cancelling the handler
        // future itself would drop it mid-execution and fire finalize-on-drop
        // guards — failing threads whose runtime children were still running
        // toward success, leaving contradictory terminal events in one braid.
        // Execution work is bounded by its own limits (thread duration,
        // runtime timeouts), not by how long an HTTP caller waited.
        let task = tokio::spawn(async move {
            route_ref
                .response_mode
                .handle(&route_ref, route_dispatch_ctx)
                .await
        });

        match tokio::time::timeout(limiter.timeout, task).await {
            Ok(Ok(Ok(resp))) => resp,
            Ok(Ok(Err(e))) => e.into_response(),
            Ok(Err(join_error)) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(serde_json::json!({
                    "error": format!("route handler task failed: {join_error}"),
                })),
            )
                .into_response(),
            Err(_) => (
                StatusCode::GATEWAY_TIMEOUT,
                axum::Json(serde_json::json!({
                    "error": "request timed out waiting for a response; the request \
                              continues server-side — check the thread's status for \
                              its real outcome",
                })),
            )
                .into_response(),
        }
    }
}
