use anyhow::Result;
use serde_json::json;

use crate::uds::protocol::{RpcRequest, RpcResponse};
use ryeos_app::state::AppState;

#[cfg(test)]
pub(crate) async fn dispatch(request: RpcRequest, state: &AppState) -> RpcResponse {
    dispatch_on(request, state, None, super::SocketSurface::Operator).await
}

#[cfg(test)]
pub(crate) async fn dispatch_on(
    request: RpcRequest,
    state: &AppState,
    peer: Option<&super::AuthenticatedUnixPeer>,
    surface: super::SocketSurface,
) -> RpcResponse {
    let lifecycle = super::ready_lifecycle_response(state);
    dispatch_with_state(request, Some(state), None, &lifecycle, peer, surface).await
}

pub(super) async fn dispatch_dynamic(
    request: RpcRequest,
    state: &super::DynamicServerState,
    peer: Option<&super::AuthenticatedUnixPeer>,
    surface: super::SocketSurface,
) -> RpcResponse {
    let lifecycle = state.lifecycle();
    let application = state.application();
    let app = application
        .as_ref()
        .map(|publication| publication.app().as_ref());
    let response = dispatch_with_state(
        request,
        app,
        application.as_deref(),
        &lifecycle,
        peer,
        surface,
    )
    .await;
    drop(application);
    response
}

async fn dispatch_with_state(
    request: RpcRequest,
    state: Option<&AppState>,
    publication: Option<&ryeos_executor::SelectedApplicationPublication>,
    lifecycle: &ryeos_node::LifecycleResponse,
    peer: Option<&super::AuthenticatedUnixPeer>,
    surface: super::SocketSurface,
) -> RpcResponse {
    // The surface gate precedes routing for both sockets. A refused method is
    // answered exactly like an unknown one, so the callback-only socket does
    // not confirm which unauthenticated methods the control socket serves.
    if !surface.admits(&request.method) {
        return unknown_method(request.request_id, &request.method);
    }
    match request.method.as_str() {
        // Local lifecycle control has no public HTTP surface.
        "lifecycle.status" => lifecycle_status(request.request_id, lifecycle, state),

        // The UDS-only health read remains available to ready clients, but it
        // is not a bootstrap alias for lifecycle.status. Before Ready,
        // only lifecycle.status is externally admitted.
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
                        super::dispatch_runtime_method_with_publication(
                            other,
                            &request.params,
                            state,
                            peer,
                            publication,
                        )
                        .await,
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

fn lifecycle_status(
    request_id: u64,
    lifecycle: &ryeos_node::LifecycleResponse,
    state: Option<&AppState>,
) -> RpcResponse {
    let window = (lifecycle.status == ryeos_node::LifecycleWireState::Running)
        .then(|| state.and_then(live_service_window))
        .flatten();
    if window.is_none() {
        return RpcResponse::ok(request_id, json!(lifecycle));
    }
    let mut lifecycle = lifecycle.clone();
    lifecycle.service_window = window;
    RpcResponse::ok(request_id, json!(lifecycle))
}

/// The running incarnation's headroom before its planned rollover, read live
/// from the same reader admission uses. A node without a native service has
/// none; a factory that already refuses service is past its window, so it
/// reports none either.
fn live_service_window(state: &AppState) -> Option<ryeos_node::ServiceWindow> {
    let reader = state
        .extensions
        .get::<ryeos_app::native_headroom::NativeServiceHeadroomReader>()?;
    let headroom = reader.current().ok()?;
    Some(service_window_at(
        headroom,
        u64::try_from(lillux::time::timestamp_millis()).unwrap_or(0) / 1_000,
    ))
}

fn service_window_at(
    headroom: ryeos_app::native_headroom::NativeServiceHeadroom,
    now_unix_secs: u64,
) -> ryeos_node::ServiceWindow {
    ryeos_node::ServiceWindow {
        seconds_until_rollover: headroom.service_seconds,
        work_steps_until_rollover: headroom.work_steps,
        rolls_at: lillux::time::iso8601_from_unix_secs(
            now_unix_secs.saturating_add(headroom.service_seconds),
        ),
    }
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
            if let Some(refusal) =
                err.downcast_ref::<ryeos_app::runtime_item_author_service::AuthorItemRefusal>()
            {
                return RpcResponse::classified_err(
                    request_id,
                    refusal.code(),
                    refusal.to_string(),
                    false,
                    json!({ "code": refusal.code() }),
                );
            }
            if let Some(refusal) =
                ryeos_app::native_headroom::NativeHeadroomInsufficient::find(&err)
            {
                let mut details = refusal.details();
                details["code"] = json!(refusal.code());
                return RpcResponse::classified_err(
                    request_id,
                    refusal.code(),
                    refusal.to_string(),
                    false,
                    details,
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

    #[test]
    fn service_window_reports_the_headroom_and_its_wall_clock_point() {
        let window = service_window_at(
            ryeos_app::native_headroom::NativeServiceHeadroom {
                work_steps: 61_000_000,
                service_seconds: 3_600,
            },
            0,
        );
        assert_eq!(window.seconds_until_rollover, 3_600);
        assert_eq!(window.work_steps_until_rollover, 61_000_000);
        assert_eq!(window.rolls_at, "1970-01-01T01:00:00Z");
    }

    #[test]
    fn native_headroom_refusal_keeps_its_typed_code_and_fields() {
        let error = anyhow::Error::new(ryeos_app::native_headroom::NativeHeadroomInsufficient {
            what: "external qualification start".to_owned(),
            dimension: ryeos_app::native_headroom::NativeHeadroomDimension::WorkSteps,
            declared: 9,
            remaining: 4,
        })
        .context("start the qualification");
        let response = rpc_result(44, Err(error));
        let error = response.error.expect("headroom refusal");
        assert_eq!(error.code, "native_headroom_insufficient");
        assert!(
            error.message.contains("before its planned rollover"),
            "{}",
            error.message
        );
    }

    #[test]
    fn author_item_refusal_keeps_its_typed_code() {
        let error = anyhow::Error::from(
            ryeos_app::runtime_item_author_service::AuthorItemRefusal::UpsertOverUnauthoredItem {
                item_ref: "knowledge:notes/x".to_string(),
                path: "/p/.ai/knowledge/notes/x.md".to_string(),
            },
        );
        let response = rpc_result(43, Err(error));
        let error = response.error.expect("author_item refusal");
        assert_eq!(error.code, "author_item_expected_digest_required");
        assert!(
            error.message.contains("knowledge:notes/x"),
            "{}",
            error.message
        );
    }

    #[test]
    fn authored_capability_refusal_keeps_its_typed_code() {
        let error = anyhow::Error::from(
            ryeos_app::runtime_item_author_service::AuthorItemRefusal::AuthoredCapabilityNotCovered {
                capability: "ryeos.execute.tool.x".to_string(),
                item_ref: "directive:a/b".to_string(),
                thread_id: "T-1".to_string(),
            },
        )
        .context("runtime.author_item");
        let response = rpc_result(44, Err(error));
        let error = response.error.expect("authored capability refusal");
        assert_eq!(error.code, "authored_capability_not_covered");
    }
}
