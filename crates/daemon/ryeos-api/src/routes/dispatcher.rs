use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use crate::api_state::ApiState;
use crate::routes::compile::RouteDispatchContext;
use crate::routes::invocation::{
    InvocationCheck, RouteInvocationContext, RouteInvocationOutput, RouteInvocationResult,
};
use crate::routes::limits::RouteLimiter;

fn dispatcher_timeout_is_disabled(timeout: Duration) -> bool {
    timeout == Duration::ZERO
}

async fn await_handler_response(
    task: tokio::task::JoinHandle<Result<Response, crate::route_error::RouteDispatchError>>,
    timeout: Duration,
) -> Response {
    if dispatcher_timeout_is_disabled(timeout) {
        match task.await {
            Ok(Ok(resp)) => resp,
            Ok(Err(e)) => e.into_response(),
            Err(join_error) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(serde_json::json!({
                    "error": format!("route handler task failed: {join_error}"),
                })),
            )
                .into_response(),
        }
    } else {
        match tokio::time::timeout(timeout, task).await {
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

pub async fn route_dispatcher(State(api_state): State<ApiState>, request: Request) -> Response {
    route_dispatcher_from_ingress(State(api_state), request, Instant::now()).await
}

/// Dispatch with the outer server's allocation-free monotonic ingress origin.
///
/// Keeping the `Instant` as an ordinary argument avoids `http::Extensions`,
/// whose type-erased insertion boxes even a Copy marker. Full launch timing
/// state remains lazy until the compiled route contract is known.
pub async fn route_dispatcher_from_ingress(
    State(api_state): State<ApiState>,
    mut request: Request,
    ingress_started_at: Instant,
) -> Response {
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
    // Launch timing allocation is intentionally scoped to the one route
    // contract that consumes it. Health checks and unrelated API traffic must
    // not pay for a UUID plus shared timing state on every request.
    let launch_timings = route.response_mode.is_dispatch_launch().then(|| {
        request
            .extensions()
            .get::<ryeos_app::launch_stage_timings::LaunchStageTimings>()
            .cloned()
            .unwrap_or_else(|| {
                let timings = ryeos_app::launch_stage_timings::LaunchStageTimings::new(
                    uuid::Uuid::new_v4().simple().to_string(),
                    ingress_started_at,
                );
                request.extensions_mut().insert(timings.clone());
                timings
            })
    });

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
        launch_timings: launch_timings.clone(),
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
        Err(e) => {
            if let Some(timings) = launch_timings.as_ref() {
                timings.record_top_level_since_start("http_request_to_auth_failure");
                timings.emit("auth_failed");
            }
            return e.into_response();
        }
    };

    let principal = match auth_result {
        RouteInvocationResult::Principal(p) => p,
        // invoke_checked guarantees Principal; any other variant is already
        // caught as an Internal error by the enforcement layer.
        _ => unreachable!("invoke_checked enforces Principal for auth"),
    };
    if let Some(timings) = launch_timings.as_ref() {
        timings.record_top_level_since_start("http_request_to_principal");
        timings.mark("principal_resolved");
    }

    let route_dispatch_ctx = RouteDispatchContext {
        captures,
        request_parts: parts,
        body_raw,
        principal,
        state: app_state,
        launch_timings,
        webhook_dedupe,
    };

    let route_ref = route.clone();

    // `timeout_ms: 0` is an explicit response-mode contract: the dispatcher
    // must not abandon the response future.  This is not limited to streaming
    // transports.  In particular, accepted execution waits only for the
    // durable launch handoff, while the launched work remains bounded by its
    // own signed execution limits.
    // Every response handler runs behind a task boundary. Dropping the HTTP
    // request (including a client disconnect) must abandon only the response,
    // never cancel admission or execution after authority has started moving.
    let task = tokio::spawn(async move {
        route_ref
            .response_mode
            .handle(&route_ref, route_dispatch_ctx)
            .await
    });

    // A non-zero timeout bounds the CLIENT's wait, never the work: hitting it
    // abandons only the task handle. Cancelling the handler future itself
    // would fire finalize-on-drop guards and could contradict runtime children
    // still working toward success. A zero timeout waits for the response-mode
    // contract instead; accepted execution uses that to return its durable
    // root handoff before the HTTP response can finish.
    await_handler_response(task, limiter.timeout).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_timeout_disables_dispatcher_abandonment_for_any_response_transport() {
        assert!(dispatcher_timeout_is_disabled(Duration::ZERO));
        assert!(!dispatcher_timeout_is_disabled(Duration::from_millis(1)));
    }

    #[tokio::test]
    async fn zero_timeout_waits_for_a_durable_accepted_response() {
        let task = tokio::spawn(async {
            tokio::time::sleep(Duration::from_millis(10)).await;
            Ok((
                StatusCode::ACCEPTED,
                axum::Json(serde_json::json!({
                    "status": "accepted",
                    "thread_id": "T-01234567-abcd-ef01-2345-6789abcdef01",
                })),
            )
                .into_response())
        });

        let response = await_handler_response(task, Duration::ZERO).await;
        assert_eq!(response.status(), StatusCode::ACCEPTED);
    }
}
