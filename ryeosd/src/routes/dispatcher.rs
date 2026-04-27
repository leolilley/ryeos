use std::time::Duration;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use crate::routes::compile::{RouteDispatchContext, VerifierRequestContext};
use crate::routes::limits::RouteLimiter;
use crate::state::AppState;

pub async fn custom_route_dispatcher(
    State(state): State<AppState>,
    request: Request,
) -> Response {
    let table = state.route_table.load_full();

    let method = request.method().clone();
    let path = request.uri().path().to_string();

    let (route, captures) = match table.match_request(&method, &path) {
        Some(r) => r,
        None => {
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
            return (StatusCode::SERVICE_UNAVAILABLE, axum::Json(serde_json::json!({"error": "too many concurrent requests"}))).into_response();
        }
        Err(tokio::sync::TryAcquireError::Closed) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, axum::Json(serde_json::json!({"error": "route semaphore closed"}))).into_response();
        }
    };

    let limiter = RouteLimiter::from_limits(&route.limits);

    if let Err(resp) = limiter.check_content_length(request.headers()) {
        return resp;
    }

    let (parts, body) = request.into_parts();

    let body_bytes = match limiter.read_bounded_body(Body::from(body)).await {
        Ok(b) => b,
        Err(resp) => return resp,
    };
    let body_raw = body_bytes.to_vec();

    let verifier_ctx = VerifierRequestContext {
        method: &method,
        path: &path,
        headers: &parts.headers,
        body_raw: &body_raw,
    };

    let principal = match route.auth.verify(&route.id, &verifier_ctx, &state) {
        Ok(p) => p,
        Err(e) => return e.into_response(),
    };

    let dispatch_ctx = RouteDispatchContext {
        captures,
        request_parts: parts,
        body_raw,
        principal,
        state,
        project_path: std::env::current_dir().ok(),
    };

    let route_ref = route.clone();

    let is_streaming = route_ref.response_mode.is_streaming();
    let no_timeout = is_streaming && limiter.timeout == Duration::ZERO;

    if no_timeout {
        match route_ref.response_mode.handle(&route_ref, dispatch_ctx).await {
            Ok(resp) => resp,
            Err(e) => e.into_response(),
        }
    } else {
        let result = tokio::time::timeout(
            limiter.timeout,
            route_ref.response_mode.handle(&route_ref, dispatch_ctx),
        )
        .await;

        match result {
            Ok(Ok(resp)) => resp,
            Ok(Err(e)) => e.into_response(),
            Err(_) => (
                StatusCode::GATEWAY_TIMEOUT,
                axum::Json(serde_json::json!({
                    "error": "request timed out",
                })),
            )
                .into_response(),
        }
    }
}
