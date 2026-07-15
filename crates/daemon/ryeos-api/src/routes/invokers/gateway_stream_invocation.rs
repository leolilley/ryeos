//! Gateway stream invoker — body-driven launch + event tail.
//!
//! Parses `item_ref` / `project_path` / `parameters` from `input`,
//! mints a thread ID, spawns dispatch, subscribes to thread events,
//! and returns `RouteInvocationResult::Stream` with lag recovery.

use serde::Deserialize;
use serde_json::Value;

use crate::route_error::RouteDispatchError;
use crate::routes::invocation::{
    CompiledRouteInvocation, PrincipalPolicy, RouteEventStream, RouteInvocationContext,
    RouteInvocationContract, RouteInvocationOutput, RouteInvocationResult,
};
use ryeos_app::event_store_service::EventReplayParams;
use ryeos_app::stream_envelope::RouteStreamEnvelope;
use ryeos_runtime::authorizer::AuthorizationPolicy;

use super::stream_helpers::*;

pub struct CompiledGatewayStreamInvocation {
    pub keep_alive_secs: u64,
}

/// Typed body shape for gateway launch requests.
///
/// Mirrors the subset of [`ExecuteRequest`] fields relevant to streaming
/// dispatch launch. All optional fields have serde defaults matching the
/// existing hard-coded behavior, so existing callers that omit them see
/// no change.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LaunchRequest {
    /// Canonical item ref to execute (e.g. "directive:my/agent").
    pub(crate) item_ref: String,
    /// Project root path for resolution.
    pub(crate) project_path: String,
    #[serde(default)]
    pub(crate) parameters: Value,
    /// Launch mode. Defaults to "inline".
    #[serde(default = "default_launch_mode")]
    pub(crate) launch_mode: String,
    /// Target site id for remote execution forwarding.
    /// v1: non-local target_site_id returns a stream_error.
    #[serde(default)]
    pub(crate) target_site_id: Option<String>,
    /// Whether to validate descriptor composition only, without execution.
    #[serde(default)]
    pub(crate) validate_only: bool,
    /// Method call: `{ method, args }`. The method selects daemon-owned
    /// behavior; the args are data. Absent for terminator/delegate kinds.
    #[serde(default)]
    pub(crate) call: Option<ryeos_engine::method_call::MethodCall>,
    #[serde(default)]
    pub(crate) usage_subject: Option<ryeos_state::UsageSubject>,
}

fn default_launch_mode() -> String {
    "inline".to_string()
}

static GATEWAY_CONTRACT: RouteInvocationContract = RouteInvocationContract {
    output: RouteInvocationOutput::Stream,
    principal: PrincipalPolicy::Optional,
};

#[axum::async_trait]
impl CompiledRouteInvocation for CompiledGatewayStreamInvocation {
    fn contract(&self) -> &'static RouteInvocationContract {
        &GATEWAY_CONTRACT
    }

    async fn invoke(
        &self,
        ctx: RouteInvocationContext,
    ) -> Result<RouteInvocationResult, RouteDispatchError> {
        // Gateway mints a new thread — Last-Event-ID is not meaningful.
        if ctx.headers.get("last-event-id").is_some() {
            return Err(RouteDispatchError::BadRequest(
                "Last-Event-ID is not supported on gateway endpoints".into(),
            ));
        }

        // Parse launch request from input (mode prepares it from body).
        let req: LaunchRequest = serde_json::from_value(ctx.input.clone())
            .map_err(|e| RouteDispatchError::BadRequest(format!("invalid request body: {e}")))?;
        if req.validate_only {
            return Err(RouteDispatchError::BadRequest(
                "validate_only is not supported by a pre-minted event stream launch".to_string(),
            ));
        }

        let item_ref =
            crate::routes::parsed_ref::ParsedItemRef::parse(&req.item_ref).map_err(|e| {
                RouteDispatchError::BadRequest(format!(
                    "invalid item_ref '{}': {}",
                    req.item_ref, e
                ))
            })?;

        // Capability check: derive the required cap from the item_ref
        // (e.g. "directive:apps/tv-tracker/ai_chat" →
        //  "ryeos.execute.directive.apps/tv-tracker/ai_chat") and check
        // via the unified Authorizer. Supports fine-grained scopes and
        // wildcards.
        {
            let principal = ctx
                .principal
                .as_ref()
                .ok_or(RouteDispatchError::Unauthorized)?;
            let subject = req
                .item_ref
                .split_once(':')
                .map(|(_, s)| s)
                .unwrap_or(&req.item_ref);
            let required_cap =
                ryeos_runtime::authorizer::canonical_cap(item_ref.kind(), subject, "execute");
            let policy = AuthorizationPolicy::require(&required_cap);
            ctx.state
                .authorizer
                .authorize(&principal.scopes, &policy)
                .map_err(|_| {
                    RouteDispatchError::Forbidden(format!(
                        "missing required capability: {}",
                        required_cap
                    ))
                })?;
        }

        let usage_subject = req.usage_subject.clone();
        let usage_subject_asserted_by = if let Some(subject) = &usage_subject {
            subject
                .validate()
                .map_err(|e| RouteDispatchError::BadRequest(e.to_string()))?;
            let principal = ctx
                .principal
                .as_ref()
                .ok_or(RouteDispatchError::Unauthorized)?;
            let required_cap = format!("ryeos.execute.on_behalf_of.{}", subject.namespace);
            let policy = AuthorizationPolicy::require(&required_cap);
            ctx.state
                .authorizer
                .authorize(&principal.scopes, &policy)
                .map_err(|_| {
                    RouteDispatchError::Forbidden(format!(
                        "missing required capability: {}",
                        required_cap
                    ))
                })?;
            Some(principal.id.clone())
        } else {
            None
        };

        let project_path =
            crate::routes::abs_path::AbsolutePathBuf::try_from_str(&req.project_path)
                .map_err(|e| RouteDispatchError::BadRequest(format!("project_path: {e}")))?;

        // The dispatch-launch stream is a fire-and-tail-until-terminal
        // contract. Non-inline launches can return before the thread is
        // terminal, and validate-only dispatch can complete without a
        // lifecycle thread at all. Reject both before admission and id minting.
        if req.launch_mode != "inline" {
            return Err(RouteDispatchError::BadRequest(format!(
                "/execute/stream supports launch_mode='inline' only; got '{}'",
                req.launch_mode
            )));
        }

        if req.validate_only {
            return Err(RouteDispatchError::BadRequest(
                "validate_only is not supported on /execute/stream; use POST /execute for validation"
                    .to_string(),
            ));
        }

        // ── Target-site guard ───────────────────────────────────────
        // v1: streaming target-site forwarding is not yet implemented.
        // Non-local target_site_id is rejected before admission and id minting.
        if let Some(ref target_site_id) = req.target_site_id {
            let current_site_id = ctx.state.threads.site_id();
            if target_site_id != current_site_id {
                return Err(RouteDispatchError::BadRequest(format!(
                    "target-site streaming is not yet supported on /execute/stream \
                         (target_site_id: '{target_site_id}'); unary target-site forwarding is \
                         currently inline-only via POST /execute"
                )));
            }
            // Self-target: normalize to local (fall through).
            tracing::debug!(
                target_site_id = %target_site_id,
                "target_site_id equals current site; normalizing to local streaming"
            );
        }

        let principal_id = ctx
            .principal
            .as_ref()
            .map(|p| p.id.clone())
            .unwrap_or_default();

        let principal_scopes = ctx
            .principal
            .as_ref()
            .map(|p| p.scopes.clone())
            .unwrap_or_default();

        // Resolve the actual persisted root (including wrapper targets), verify
        // it, and capture its policy before exposing an id to the stream.
        let preflight = crate::routes::launch::preflight_dispatch_launch(
            &ctx.state,
            &item_ref,
            &project_path,
            &req.parameters,
            &principal_id,
            &principal_scopes,
            req.call.clone(),
            &req.launch_mode,
            req.validate_only,
            usage_subject.as_ref(),
            usage_subject_asserted_by.as_deref(),
        )
        .map_err(|error| {
            RouteDispatchError::BadRequest(format!("stream root launch admission failed: {error}"))
        })?;
        if !preflight.class.persists_pre_minted_root() {
            return Err(RouteDispatchError::BadRequest(
                "stream launch requires execution that persists a pre-minted thread root"
                    .to_string(),
            ));
        }
        let root_admission = preflight.root_admission.ok_or_else(|| {
            RouteDispatchError::Internal(
                "threaded dispatch preflight returned no root admission".to_string(),
            )
        })?;
        let mut options = crate::routes::launch::DispatchLaunchOptions::admitted(root_admission)
            .map_err(|error| {
                RouteDispatchError::Internal(format!(
                    "validated stream contract rejected at dispatch boundary: {error:#}"
                ))
            })?;
        options.launch_mode = req.launch_mode;
        options.target_site_id = req.target_site_id;
        options.validate_only = req.validate_only;
        options.usage_subject = usage_subject;
        options.usage_subject_asserted_by = usage_subject_asserted_by;
        options.call = req.call;

        let thread_id = ryeos_app::thread_lifecycle::new_thread_id();

        let route_id: String = ctx.route_id.to_string();

        let span = tracing::info_span!(
            "dispatch_launch_sse",
            route_id = route_id.as_str(),
            thread_id = thread_id.as_str(),
            item_ref_kind = item_ref.kind(),
        );

        let hub = ctx.state.event_streams.clone();
        // Subscribe before launch so no live event is missed; the guard
        // (moved into the stream below) reclaims the sender at stream end.
        let sub = ryeos_app::event_stream::HubSubscription::new(hub, &thread_id);

        let launch_provenance = ryeos_app::execution_provenance::ExecutionProvenance::root_live_fs(
            options.project_path().to_path_buf(),
            ctx.state.engine.clone(),
        );
        let mut launch_handle = crate::routes::launch::spawn_dispatch_launch(
            &ctx.state,
            item_ref,
            req.parameters,
            principal_id,
            principal_scopes,
            thread_id.clone(),
            launch_provenance,
            options,
        );

        let events_svc = ctx.state.events.clone();
        let state_store_clone = ctx.state.state_store.clone();
        let thread_id_for_stream = thread_id.clone();
        let keep_alive_secs = self.keep_alive_secs;

        let stream = async_stream::stream! {
            let _guard = span.enter();
            // Move the subscription guard (which owns the receiver) into the
            // stream so the sender is reclaimed when the stream ends.
            let mut sub = sub;
            yield Ok(
                RouteStreamEnvelope::new(
                    "stream_started",
                    serde_json::json!({"thread_id": thread_id_for_stream}),
                )
            );

            let mut current_max: i64 = 0;
            let replay_batch_size = REPLAY_BATCH_SIZE;

            loop {
                tokio::select! {
                    recv_result = sub.recv() => {
                        match recv_result {
                            Ok(ev) => {
                                let event_type = ev.event_type.clone();
                                if is_ephemeral(&ev) {
                                    yield Ok(envelope_for_persisted(&ev));
                                    continue;
                                }
                                if ev.chain_seq > current_max {
                                    current_max = ev.chain_seq;
                                    yield Ok(envelope_for_persisted(&ev));
                                }
                                if is_terminal(&event_type) {
                                    return;
                                }
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                                let mut lag_max = current_max;
                                let mut lag_error: Option<String> = None;
                                let mut next_after: Option<i64> =
                                    if current_max > 0 { Some(current_max) } else { None };
                                loop {
                                    let page = events_svc.replay(&EventReplayParams {
                                        chain_root_id: None,
                                        thread_id: Some(thread_id_for_stream.clone()),
                                        after_chain_seq: next_after,
                                        limit: replay_batch_size,
                                    });
                                    match page {
                                        Ok(page_result) => {
                                            if page_result.events.is_empty() {
                                                break;
                                            }
                                            for ev in &page_result.events {
                                                if ev.chain_seq > lag_max {
                                                    lag_max = ev.chain_seq;
                                                    yield Ok(envelope_for_persisted(ev));
                                                    if is_terminal(&ev.event_type) {
                                                        return;
                                                    }
                                                }
                                            }
                                            if page_result.next_cursor.is_none() {
                                                break;
                                            }
                                            next_after = Some(lag_max);
                                        }
                                        Err(e) => {
                                            lag_error = Some(format!("lag replay failed: {e}"));
                                            break;
                                        }
                                    }
                                }

                                if let Some(err_msg) = lag_error {
                                    let thread = state_store_clone.get_thread(&thread_id_for_stream);
                                    if let Ok(Some(detail)) = thread {
                                        if is_terminal_status(&detail.status) {
                                            return;
                                        }
                                    }
                                    yield Ok(error_envelope("replay_failed", &err_msg));
                                    return;
                                }

                                current_max = lag_max;

                                tracing::info!(
                                    thread_id = %thread_id_for_stream,
                                    lagged = n,
                                    "dispatch_launch envelope subscriber lag recovery complete"
                                );
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                return;
                            }
                        }
                    }
                    join_result = &mut launch_handle => {
                        match join_result {
                            Ok(Ok(())) => {
                                loop {
                                    match sub.try_recv() {
                                        Ok(ev) => {
                                            if is_ephemeral(&ev) {
                                                yield Ok(envelope_for_persisted(&ev));
                                                continue;
                                            }
                                            if ev.chain_seq > current_max {
                                                current_max = ev.chain_seq;
                                                yield Ok(envelope_for_persisted(&ev));
                                            }
                                            if is_terminal(&ev.event_type) {
                                                return;
                                            }
                                        }
                                        Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
                                        Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => break,
                                        Err(tokio::sync::broadcast::error::TryRecvError::Closed) => return,
                                    }
                                }

                                // Post-launch drain: replay any events the broadcast
                                // didn't deliver from the durable store.
                                let mut next_after: Option<i64> =
                                    if current_max > 0 { Some(current_max) } else { None };
                                let mut saw_terminal = false;
                                loop {
                                    let page = events_svc.replay(&EventReplayParams {
                                        chain_root_id: None,
                                        thread_id: Some(thread_id_for_stream.clone()),
                                        after_chain_seq: next_after,
                                        limit: replay_batch_size,
                                    });
                                    match page {
                                        Ok(page_result) => {
                                            if page_result.events.is_empty() {
                                                break;
                                            }
                                            for ev in &page_result.events {
                                                if ev.chain_seq > current_max {
                                                    current_max = ev.chain_seq;
                                                    yield Ok(envelope_for_persisted(ev));
                                                    if is_terminal(&ev.event_type) {
                                                        saw_terminal = true;
                                                        break;
                                                    }
                                                }
                                            }
                                            if saw_terminal {
                                                return;
                                            }
                                            if page_result.next_cursor.is_none() {
                                                break;
                                            }
                                            next_after = Some(current_max);
                                        }
                                        Err(e) => {
                                            yield Ok(error_envelope("post_launch_replay_failed", &format!("post-launch replay failed: {e}")));
                                            return;
                                        }
                                    }
                                }
                                let detail = state_store_clone.get_thread(&thread_id_for_stream);
                                if let Ok(Some(d)) = detail {
                                    if is_terminal_status(&d.status) {
                                        return;
                                    }
                                }
                                yield Ok(error_envelope("thread_not_terminal", "launch completed but thread is not terminal"));
                                return;
                            }
                            Ok(Err(e)) => {
                                let extras = match &e {
                                    crate::routes::launch::LaunchSpawnError::Dispatch(de) => {
                                        let payload = ryeos_executor::structured_error::StructuredErrorPayload::from(de);
                                        // Strip `code` and `error` so the helper's explicit args win.
                                        let mut value = payload.to_value();
                                        if let Some(map) = value.as_object_mut() {
                                            map.remove("code");
                                            map.remove("error");
                                        }
                                        Some(value)
                                    }
                                    _ => None,
                                };
                                yield Ok(error_envelope_with(
                                    e.code(),
                                    &format!("launch failed: {e}"),
                                    extras,
                                ));
                                return;
                            }
                            Err(_) => {
                                yield Ok(error_envelope("task_panicked", "launch task panicked"));
                                return;
                            }
                        }
                    }
                }
            }
        };

        Ok(RouteInvocationResult::Stream(RouteEventStream {
            events: Box::pin(stream),
            keep_alive_secs,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_request_minimal_fields_deserialize() {
        let json = serde_json::json!({
            "item_ref": "directive:foo/bar",
            "project_path": "/tmp/project",
            "parameters": {}
        });
        let req: LaunchRequest = serde_json::from_value(json).unwrap();
        assert_eq!(req.item_ref, "directive:foo/bar");
        assert_eq!(req.project_path, "/tmp/project");
        assert_eq!(req.launch_mode, "inline");
        assert_eq!(req.target_site_id, None);
        assert!(!req.validate_only);
        assert!(req.call.is_none());
    }

    #[test]
    fn launch_request_all_fields_deserialize() {
        let json = serde_json::json!({
            "item_ref": "tool:x/y",
            "project_path": "/home/me/project",
            "parameters": {"key": "val"},
            "launch_mode": "detached",
            "target_site_id": "site:remote",
            "validate_only": true,
            "call": {"method": "run", "args": {"arg": 42}}
        });
        let req: LaunchRequest = serde_json::from_value(json).unwrap();
        assert_eq!(req.item_ref, "tool:x/y");
        assert_eq!(req.launch_mode, "detached");
        assert_eq!(req.target_site_id.as_deref(), Some("site:remote"));
        assert!(req.validate_only);
        let call = req.call.as_ref().expect("call present");
        assert_eq!(call.method(), Some("run"));
        assert_eq!(call.args().unwrap()["arg"], 42);
    }

    #[test]
    fn launch_request_rejects_unknown_fields() {
        let json = serde_json::json!({
            "item_ref": "directive:x",
            "project_path": "/tmp/p",
            "parameters": {},
            "bogus_field": true
        });
        let result = serde_json::from_value::<LaunchRequest>(json);
        assert!(
            result.is_err(),
            "expected deny_unknown_fields to reject bogus_field"
        );
        let msg = format!("{:?}", result.unwrap_err());
        assert!(
            msg.contains("bogus_field"),
            "error should mention the unknown field: {msg}"
        );
    }

    #[test]
    fn launch_request_defaults_are_inline_local() {
        let json = serde_json::json!({
            "item_ref": "directive:x",
            "project_path": "/tmp/p",
            "parameters": {}
        });
        let req: LaunchRequest = serde_json::from_value(json).unwrap();
        assert_eq!(req.launch_mode, "inline");
        assert_eq!(req.target_site_id, None);
        assert!(!req.validate_only);
        assert!(req.call.is_none());
    }
}
