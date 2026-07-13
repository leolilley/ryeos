use anyhow::Result;
use serde_json::json;

use crate::uds::protocol::{RpcRequest, RpcResponse};
use ryeos_app::state::AppState;

pub(crate) async fn dispatch(request: RpcRequest, state: &AppState) -> RpcResponse {
    match request.method.as_str() {
        // Daemon health is intentionally lightweight and ungated.
        "system.health" => system_health(request.request_id, state),

        // Local lifecycle control has no public HTTP surface.
        "lifecycle.status" => lifecycle_status(request.request_id, state),
        "lifecycle.shutdown" => lifecycle_shutdown(request.request_id),

        // Runtime callbacks retain their token-gated service dispatcher.
        other if other.starts_with("runtime.") => rpc_result(
            request.request_id,
            super::dispatch_runtime_method(other, &request.params, state).await,
        ),

        other => unknown_method(request.request_id, other),
    }
}

fn system_health(request_id: u64, state: &AppState) -> RpcResponse {
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
            "thread_projection": thread_projection,
        }),
    )
}

fn lifecycle_status(request_id: u64, state: &AppState) -> RpcResponse {
    RpcResponse::ok(
        request_id,
        json!({
            "status": "running",
            "pid": std::process::id(),
            "version": env!("CARGO_PKG_VERSION"),
            "started_at": &state.started_at_iso,
            "bind": state.config.bind.to_string(),
            "uds_path": state.config.uds_path.display().to_string(),
            "app_root": state.config.app_root.display().to_string(),
            "thread_projection": state.state_store.projection_health_snapshot(),
        }),
    )
}

fn lifecycle_shutdown(request_id: u64) -> RpcResponse {
    crate::request_shutdown();
    RpcResponse::ok(request_id, json!({ "accepted": true }))
}

fn unknown_method(request_id: u64, method: &str) -> RpcResponse {
    RpcResponse::err(
        request_id,
        "unknown_method",
        format!("unknown rpc method: {method}"),
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
