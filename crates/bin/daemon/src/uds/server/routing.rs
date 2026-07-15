use anyhow::Result;
use serde_json::json;

use crate::uds::protocol::{RpcRequest, RpcResponse};
use ryeos_app::state::AppState;

pub(crate) async fn dispatch(request: RpcRequest, state: &AppState) -> RpcResponse {
    let lifecycle = super::ready_lifecycle_response(state);
    dispatch_with_state(request, Some(state), &lifecycle, None).await
}

pub(super) async fn dispatch_dynamic(
    request: RpcRequest,
    state: &super::DynamicServerState,
) -> RpcResponse {
    let lifecycle = state.lifecycle();
    let application = state.application();
    dispatch_with_state(request, application.as_deref(), &lifecycle, Some(state)).await
}

async fn dispatch_with_state(
    request: RpcRequest,
    state: Option<&AppState>,
    lifecycle: &ryeos_node::LifecycleResponse,
    dynamic: Option<&super::DynamicServerState>,
) -> RpcResponse {
    match request.method.as_str() {
        // Local lifecycle control has no public HTTP surface.
        "lifecycle.status" => lifecycle_status(request.request_id, lifecycle),
        "lifecycle.shutdown" => lifecycle_shutdown(request.request_id, dynamic),

        // The UDS-only health read remains available to ready clients, but it
        // is not a bootstrap alias for lifecycle.status. Before Ready,
        // only the two lifecycle methods above are externally admitted.
        "system.health" => {
            if lifecycle.status == ryeos_node::LifecycleWireState::Running && lifecycle.ready {
                system_health(request.request_id, state, lifecycle)
            } else {
                application_unavailable(request.request_id, lifecycle)
            }
        }

        // Runtime callbacks retain their token-gated service dispatcher.
        other if other.starts_with("runtime.") => {
            if lifecycle.status == ryeos_node::LifecycleWireState::Failed {
                application_unavailable(request.request_id, lifecycle)
            } else {
                match state {
                    Some(state) => rpc_result(
                        request.request_id,
                        super::dispatch_runtime_method(other, &request.params, state).await,
                    ),
                    None => application_unavailable(request.request_id, lifecycle),
                }
            }
        }

        other => unknown_method(request.request_id, other),
    }
}

fn system_health(
    request_id: u64,
    state: Option<&AppState>,
    lifecycle: &ryeos_node::LifecycleResponse,
) -> RpcResponse {
    if lifecycle.status == ryeos_node::LifecycleWireState::Failed {
        return RpcResponse::ok(
            request_id,
            json!({
                "status": "failed",
                "ready": false,
                "startup": &lifecycle.startup,
            }),
        );
    }
    let Some(state) = state else {
        return RpcResponse::ok(
            request_id,
            json!({
                "status": "starting",
                "ready": false,
                "startup": &lifecycle.startup,
            }),
        );
    };
    let thread_projection = state.state_store.projection_health_snapshot();
    let status = if thread_projection.status
        == ryeos_app::projection_health::ThreadProjectionState::Current
    {
        "ok"
    } else {
        "degraded"
    };
    RpcResponse::ok(
        request_id,
        json!({
            "status": status,
            "ready": lifecycle.ready,
            "startup": &lifecycle.startup,
            "thread_projection": thread_projection,
        }),
    )
}

fn lifecycle_status(request_id: u64, lifecycle: &ryeos_node::LifecycleResponse) -> RpcResponse {
    RpcResponse::ok(request_id, json!(lifecycle))
}

fn lifecycle_shutdown(request_id: u64, dynamic: Option<&super::DynamicServerState>) -> RpcResponse {
    match dynamic {
        Some(dynamic) => dynamic.request_shutdown(),
        None => crate::request_shutdown(),
    }
    RpcResponse::ok(request_id, json!({ "accepted": true }))
}

fn unknown_method(request_id: u64, method: &str) -> RpcResponse {
    RpcResponse::err(
        request_id,
        "unknown_method",
        format!("unknown rpc method: {method}"),
    )
}

fn application_unavailable(
    request_id: u64,
    lifecycle: &ryeos_node::LifecycleResponse,
) -> RpcResponse {
    let failed = lifecycle.status == ryeos_node::LifecycleWireState::Failed;
    RpcResponse::classified_err(
        request_id,
        if failed {
            "node_startup_failed"
        } else {
            "node_starting"
        },
        if failed {
            lifecycle
                .error
                .as_deref()
                .unwrap_or("daemon startup failed")
        } else {
            "daemon application state is not ready"
        },
        !failed,
        json!({
            "phase": lifecycle.startup.phase,
            "sequence": lifecycle.startup.sequence,
            "elapsed_ms": lifecycle.startup.elapsed_ms,
            "retry_after_ms": if failed { None } else { Some(250u64) },
            "startup": &lifecycle.startup,
        }),
    )
}

fn rpc_result(request_id: u64, result: Result<serde_json::Value>) -> RpcResponse {
    match result {
        Ok(value) => RpcResponse::ok(request_id, value),
        // `{:#}` walks the anyhow cause chain so callers see the root cause,
        // not only a top-level handler context line.
        Err(err) => {
            if let Some(dispatch) =
                err.downcast_ref::<ryeos_executor::dispatch_error::DispatchError>()
            {
                return RpcResponse::classified_err(
                    request_id,
                    dispatch.code(),
                    dispatch.to_string(),
                    dispatch.retryable(),
                    json!({ "code": dispatch.code() }),
                );
            }
            RpcResponse::err(request_id, "request_failed", format!("{err:#}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_method_preserves_code_message_and_request_id() {
        let response = unknown_method(41, "nonexistent.method");
        assert_eq!(response.request_id, 41);
        let error = response.error.expect("unknown method error");
        assert_eq!(error.code, "unknown_method");
        assert_eq!(error.message, "unknown rpc method: nonexistent.method");
    }

    #[test]
    fn runtime_failure_preserves_full_error_chain() {
        let error = anyhow::anyhow!("root cause").context("handler context");
        let response = rpc_result(42, Err(error));
        assert_eq!(response.request_id, 42);
        let error = response.error.expect("request failure");
        assert_eq!(error.code, "request_failed");
        assert_eq!(error.message, "handler context: root cause");
    }
}
