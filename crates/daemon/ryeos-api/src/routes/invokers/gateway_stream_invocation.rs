//! Gateway stream invoker — body-driven launch + event tail.
//!
//! Parses `item_ref` / `project_path` / `parameters` from `input`,
//! mints a thread ID, spawns dispatch, subscribes to thread events,
//! and returns `RouteInvocationResult::Stream` with lag recovery.

use serde::Deserialize;
use serde_json::Value;

use crate::route_error::RouteDispatchError;
use crate::routes::invocation::{
    authenticated_execution_origin, CompiledRouteInvocation, PrincipalPolicy, RouteEventStream,
    RouteInvocationContext, RouteInvocationContract, RouteInvocationOutput, RouteInvocationResult,
};
use crate::routes::response_modes::execute_mode::{
    preauthorize_execution_policy, resolve_execution_contract, resolve_project_context_off_thread,
    ProjectRootNormalization, ResolveProjectContextRequest,
};
use ryeos_app::event_store_service::EventReplayParams;
use ryeos_app::stream_envelope::RouteStreamEnvelope;
use ryeos_runtime::authorizer::AuthorizationPolicy;

use super::stream_helpers::*;

pub struct CompiledGatewayStreamInvocation {
    pub keep_alive_secs: u64,
}

/// Streaming keeps request-scoped launches owned by the SSE request. Tokio
/// join handles detach when dropped, so an explicit abort guard is required;
/// daemon-owned launches deliberately carry no abort handle.
struct RequestScopedLaunchGuard(Option<tokio::task::AbortHandle>);

impl Drop for RequestScopedLaunchGuard {
    fn drop(&mut self) {
        if let Some(handle) = self.0.take() {
            handle.abort();
        }
    }
}

/// Typed body shape for gateway launch requests.
///
/// Mirrors the subset of [`ExecuteRequest`] fields relevant to streaming
/// dispatch launch.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LaunchRequest {
    /// Canonical item ref to execute (e.g. "directive:my/agent").
    pub(crate) item_ref: String,
    pub(crate) ref_bindings: std::collections::BTreeMap<String, String>,
    /// Project root path for resolution.
    pub(crate) project_path: String,
    #[serde(default)]
    pub(crate) parameters: Value,
    pub(crate) execution_policy: ryeos_app::execution_policy::ExecutionPolicy,
    #[serde(skip)]
    pub(crate) launch_mode: String,
    /// Target site id for remote execution forwarding.
    /// Non-local target_site_id returns a stream_error.
    #[serde(skip)]
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

fn handoff_error_envelope(
    failure: ryeos_executor::execution::launch::LaunchHandoffFailure,
) -> RouteStreamEnvelope {
    error_envelope_with(&failure.code, &failure.message, Some(failure.body))
}

fn dispatch_error_envelope(
    error: ryeos_executor::dispatch_error::DispatchError,
) -> RouteStreamEnvelope {
    let mut payload =
        ryeos_executor::structured_error::StructuredErrorPayload::from(&error).to_value();
    if let Some(map) = payload.as_object_mut() {
        map.remove("code");
        map.remove("error");
    }
    let code = error.code().to_owned();
    let message = error.to_string();
    error_envelope_with(&code, &message, Some(payload))
}

fn pre_spawn_dispatch_error(
    keep_alive_secs: u64,
    error: ryeos_executor::dispatch_error::DispatchError,
) -> RouteInvocationResult {
    let envelope = dispatch_error_envelope(error);
    let stream = async_stream::stream! {
        yield Ok(envelope);
    };
    RouteInvocationResult::Stream(RouteEventStream {
        events: Box::pin(stream),
        keep_alive_secs,
    })
}

fn completed_launch_error_envelope(
    result: Result<Result<(), crate::routes::launch::LaunchSpawnError>, tokio::task::JoinError>,
) -> RouteStreamEnvelope {
    match result {
        Ok(Err(crate::routes::launch::LaunchSpawnError::Dispatch(error))) => {
            dispatch_error_envelope(error)
        }
        Ok(Err(error)) => error_envelope(error.code(), &error.to_string()),
        Ok(Ok(())) => error_envelope(
            "launch_handoff_missing",
            "launch completed without authoritative handoff",
        ),
        Err(error) => error_envelope("launch_task_failed", &error.to_string()),
    }
}

fn execution_planning_envelope(launch_id: &str) -> RouteStreamEnvelope {
    RouteStreamEnvelope::new(
        "execution_planning",
        serde_json::json!({"launch_id": launch_id}),
    )
}

fn map_launch_planning_reservation_error(
    error: ryeos_app::state_store::LaunchPlanningReservationError,
) -> RouteDispatchError {
    match error {
        ryeos_app::state_store::LaunchPlanningReservationError::CapacityExceeded(_) => {
            RouteDispatchError::ServiceUnavailable {
                code: "launch_planning_capacity_exceeded".to_string(),
                message: "launch planning capacity is temporarily unavailable".to_string(),
            }
        }
        ryeos_app::state_store::LaunchPlanningReservationError::Internal(error) => {
            RouteDispatchError::Internal(format!(
                "persist stream launch planning admission: {error:#}"
            ))
        }
    }
}

fn launch_handoff_identity_error(
    expected_thread_id: &str,
    handed_off_thread_id: &str,
) -> Option<RouteStreamEnvelope> {
    (expected_thread_id != handed_off_thread_id).then(|| {
        error_envelope(
            "launch_handoff_identity_mismatch",
            "authoritative handoff returned a different thread identity",
        )
    })
}

async fn await_launch_handoff(
    launch_handle: &mut tokio::task::JoinHandle<
        Result<(), crate::routes::launch::LaunchSpawnError>,
    >,
    ready: tokio::sync::oneshot::Receiver<ryeos_executor::execution::launch::LaunchHandoffResult>,
) -> Result<String, RouteStreamEnvelope> {
    tokio::select! {
        biased;
        readiness = ready => match readiness {
            Ok(Ok(ready_thread_id)) => Ok(ready_thread_id),
            Ok(Err(failure)) => Err(handoff_error_envelope(failure)),
            Err(_) => Err(completed_launch_error_envelope((&mut *launch_handle).await)),
        },
        result = &mut *launch_handle => Err(completed_launch_error_envelope(result)),
    }
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
        let mut req: LaunchRequest = serde_json::from_value(ctx.input.clone())
            .map_err(|e| RouteDispatchError::BadRequest(format!("invalid request body: {e}")))?;
        req.execution_policy
            .validate()
            .map_err(|error| RouteDispatchError::BadRequest(error.to_string()))?;
        req.launch_mode = match req.execution_policy.response {
            ryeos_app::execution_policy::ExecutionResponse::Wait => "wait".to_string(),
            ryeos_app::execution_policy::ExecutionResponse::Accepted => "accepted".to_string(),
        };
        req.target_site_id = match &req.execution_policy.target {
            ryeos_app::execution_policy::ExecutionTarget::Here => None,
            ryeos_app::execution_policy::ExecutionTarget::Site { site_id } => Some(site_id.clone()),
        };
        if !matches!(
            &req.execution_policy.project,
            ryeos_app::execution_policy::ProjectExecutionPolicy::LiveDirect { .. }
        ) {
            return Err(RouteDispatchError::BadRequest(
                "/execute/stream requires explicit live_direct project policy".to_string(),
            ));
        }
        let ref_binding_validation_timer = ctx
            .launch_timings
            .as_ref()
            .map(|timings| timings.top_level("route_ref_binding_validation"));
        let ref_binding_validation =
            ryeos_executor::execution::launch_preparation::validate_ref_bindings(&req.ref_bindings);
        drop(ref_binding_validation_timer);
        if let Err(error) = ref_binding_validation {
            return Ok(pre_spawn_dispatch_error(self.keep_alive_secs, error));
        }
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
            let ref_binding_authorization_timer = ctx
                .launch_timings
                .as_ref()
                .map(|timings| timings.top_level("route_ref_binding_authorization"));
            for (name, bound_ref) in &req.ref_bindings {
                let canonical = ryeos_engine::canonical_ref::CanonicalRef::parse(bound_ref)
                    .map_err(|error| {
                        RouteDispatchError::BadRequest(format!(
                            "invalid ref_bindings.{name}: {error}"
                        ))
                    })?;
                let required = ryeos_runtime::authorizer::canonical_cap(
                    &canonical.kind,
                    &canonical.bare_id,
                    "execute",
                );
                let policy = AuthorizationPolicy::require(&required);
                ctx.state
                    .authorizer
                    .authorize(&principal.scopes, &policy)
                    .map_err(|_| {
                        RouteDispatchError::Forbidden(format!(
                            "missing required capability for ref binding '{name}': {required}"
                        ))
                    })?;
            }
            drop(ref_binding_authorization_timer);
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
        // contract. Non-waiting launches can return before the thread is
        // terminal, and validate-only dispatch can complete without a
        // lifecycle thread at all. Reject both before admission and id minting.
        if req.launch_mode != "wait" {
            return Err(RouteDispatchError::BadRequest(format!(
                "/execute/stream supports launch_mode='wait' only; got '{}'",
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
                         currently wait-only via POST /execute"
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
        let execution_origin_site_id =
            authenticated_execution_origin(ctx.principal.as_ref(), ctx.state.threads.site_id());

        // Policy capability checks precede canonicalization or any other
        // caller-selected project filesystem access.
        preauthorize_execution_policy(&req.execution_policy, &principal_scopes, &ctx.state)
            .map_err(|error| RouteDispatchError::BadRequest(error.to_string()))?;

        let thread_id = ryeos_app::thread_lifecycle::new_thread_id();
        if let Some(timings) = ctx.launch_timings.as_ref() {
            timings.set_launch_dimensions(item_ref.kind(), "gateway_stream");
            timings.bind_thread_id(&thread_id);
        }
        let project_source = ryeos_executor::execution::project_source::ProjectSource::LiveFs;
        let project_context_timer = ctx
            .launch_timings
            .as_ref()
            .map(|timings| timings.top_level("project_context_resolution"));
        let project_context_result =
            resolve_project_context_off_thread(ResolveProjectContextRequest {
                state: ctx.state.clone(),
                source: project_source.clone(),
                project_path: project_path.as_path().to_path_buf(),
                principal_id: principal_id.clone(),
                checkout_id: format!("stream-{thread_id}"),
                pinned_realization:
                    ryeos_executor::execution::project_source::PinnedContextRealization::Cow,
                normalization: ProjectRootNormalization::CanonicalizeLive,
                launch_timings: ctx.launch_timings.clone(),
            })
            .await;
        drop(project_context_timer);
        let mut project_ctx = project_context_result.map_err(|error| {
            if let Some(timings) = ctx.launch_timings.as_ref() {
                timings.emit("gateway_project_context_failed");
            }
            RouteDispatchError::BadRequest(format!("resolve stream project: {error}"))
        })?;
        let resolved_contract = resolve_execution_contract(
            &req.execution_policy,
            &project_source,
            &project_ctx,
            project_ctx.temp_dir.clone(),
            None,
            &principal_id,
            &principal_scopes,
            &ctx.state,
        )
        .map_err(|error| {
            if let Some(timings) = ctx.launch_timings.as_ref() {
                timings.emit("gateway_execution_contract_failed");
            }
            RouteDispatchError::BadRequest(format!("resolve stream execution authority: {error}"))
        })?;

        // Resolve the actual persisted root (including wrapper targets), verify
        // it, and capture its policy before exposing an id to the stream.
        let preflight_timer = ctx
            .launch_timings
            .as_ref()
            .map(|timings| timings.top_level("preflight_admission"));
        let preflight_result = crate::routes::launch::preflight_dispatch_launch_off_thread(
            crate::routes::launch::OwnedDispatchPreflight {
                state: ctx.state.clone(),
                item_ref: item_ref.clone(),
                project_path: project_ctx.effective_path.clone(),
                request_engine: project_ctx.request_engine.clone(),
                parameters: req.parameters.clone(),
                ref_bindings: req.ref_bindings.clone(),
                principal_id: principal_id.clone(),
                principal_scopes: principal_scopes.clone(),
                origin_site_id: execution_origin_site_id.clone(),
                call: req.call.clone(),
                launch_mode: req.launch_mode.clone(),
                validate_only: req.validate_only,
                usage_subject: usage_subject.clone(),
                usage_subject_asserted_by: usage_subject_asserted_by.clone(),
                launch_timings: ctx.launch_timings.clone(),
            },
        )
        .await;
        drop(preflight_timer);
        let preflight = preflight_result.map_err(|error| {
            if let Some(timings) = ctx.launch_timings.as_ref() {
                timings.emit("gateway_preflight_failed");
            }
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
        let mut options = crate::routes::launch::DispatchLaunchOptions::admitted(
            root_admission,
            &project_ctx.effective_path,
            req.ref_bindings,
            resolved_contract.lifecycle_authority,
        )
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
        options.launch_timings = ctx.launch_timings.clone();
        let request_scoped = resolved_contract.lifecycle_authority.ownership
            == ryeos_state::objects::ExecutionOwnershipAuthority::RequestScoped;
        options = options.retain_captured_generation(project_ctx.take_captured_generation());

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

        // Commit the owner-bound cancellation handle only once every fallible
        // launch-assembly step has succeeded, and immediately before task
        // spawn. From this point, the task-exit guard settles every pre-bind
        // failure/abort/unwind path.
        let planning_commit_timer = ctx
            .launch_timings
            .as_ref()
            .map(|timings| timings.top_level("launch_planning_commit"));
        let launch_id = ctx
            .state
            .state_store
            .reserve_launch_planning(&thread_id, &principal_id)
            .map_err(|error| {
                if let Some(timings) = ctx.launch_timings.as_ref() {
                    timings.emit("gateway_planning_commit_failed");
                }
                map_launch_planning_reservation_error(error)
            })?;
        drop(planning_commit_timer);
        if let Some(timings) = ctx.launch_timings.as_ref() {
            timings.mark("launch_planning_committed");
        }
        let launch_provenance = resolved_contract.provenance;
        let (mut launch_handle, ready) = crate::routes::launch::spawn_dispatch_launch_with_handoff(
            &ctx.state,
            item_ref,
            req.parameters,
            principal_id,
            principal_scopes,
            thread_id.clone(),
            launch_provenance,
            options,
        );
        let launch_scope = RequestScopedLaunchGuard(if request_scoped {
            Some(launch_handle.abort_handle())
        } else {
            None
        });

        let events_svc = ctx.state.events.clone();
        let state_store_clone = ctx.state.state_store.clone();
        let thread_id_for_stream = thread_id.clone();
        let keep_alive_secs = self.keep_alive_secs;
        let launch_timings = ctx.launch_timings.clone();

        let stream = async_stream::stream! {
            // Keep the span alive for the stream lifetime without entering it
            // across awaits. An entered tracing guard is thread-local and can
            // otherwise attribute unrelated task work while this stream waits.
            let _span_lifetime = span;
            let _launch_scope = launch_scope;
            // Move the subscription guard (which owns the receiver) into the
            // stream so the sender is reclaimed when the stream ends.
            let mut sub = sub;

            if let Some(timings) = launch_timings.as_ref() {
                timings.mark("execution_planning_yielded");
                timings.record_top_level_from_milestone(
                    "planning_commit_to_execution_planning_yield",
                    "launch_planning_committed",
                );
                timings.emit("execution_planning_yielded");
            }
            yield Ok(execution_planning_envelope(&launch_id));

            let ready_thread_id = match await_launch_handoff(&mut launch_handle, ready).await {
                Ok(ready_thread_id) => {
                    if let Some(timings) = launch_timings.as_ref() {
                        timings.mark("launch_handoff_observed");
                    }
                    ready_thread_id
                }
                Err(envelope) => {
                    if let Some(timings) = launch_timings.as_ref() {
                        timings.emit("gateway_stream_launch_failed");
                    }
                    yield Ok(envelope);
                    return;
                }
            };
            if let Some(envelope) =
                launch_handoff_identity_error(&thread_id, &ready_thread_id)
            {
                if let Some(timings) = launch_timings.as_ref() {
                    timings.mark("launch_handoff_identity_mismatch");
                    timings.emit("gateway_stream_launch_failed");
                }
                yield Ok(envelope);
                return;
            }

            if let Some(timings) = launch_timings.as_ref() {
                timings.mark("stream_started_yielded");
                timings.record_top_level_from_milestone(
                    "handoff_to_stream_started_yield",
                    "launch_handoff_observed",
                );
                timings.emit("gateway_stream_started");
            }
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

    fn local_live_policy() -> Value {
        serde_json::to_value(ryeos_app::execution_policy::ExecutionPolicy::local_live(
            ryeos_app::execution_policy::ExecutionResponse::Wait,
        ))
        .unwrap()
    }

    #[test]
    fn launch_request_minimal_fields_deserialize() {
        let json = serde_json::json!({
            "item_ref": "directive:foo/bar",
            "ref_bindings": {},
            "project_path": "/tmp/project",
            "parameters": {},
            "execution_policy": local_live_policy()
        });
        let req: LaunchRequest = serde_json::from_value(json).unwrap();
        assert_eq!(req.item_ref, "directive:foo/bar");
        assert_eq!(req.project_path, "/tmp/project");
        assert!(req.launch_mode.is_empty());
        assert_eq!(req.target_site_id, None);
        assert!(!req.validate_only);
        assert!(req.call.is_none());
    }

    #[test]
    fn launch_request_all_fields_deserialize() {
        let json = serde_json::json!({
            "item_ref": "tool:x/y",
            "ref_bindings": {"guard": "tool:guard/check"},
            "project_path": "/home/me/project",
            "parameters": {"key": "val"},
            "execution_policy": local_live_policy(),
            "validate_only": true,
            "call": {"method": "run", "args": {"arg": 42}}
        });
        let req: LaunchRequest = serde_json::from_value(json).unwrap();
        assert_eq!(req.item_ref, "tool:x/y");
        assert!(req.launch_mode.is_empty());
        assert_eq!(req.target_site_id, None);
        assert_eq!(
            req.execution_policy.response,
            ryeos_app::execution_policy::ExecutionResponse::Wait
        );
        assert!(req.validate_only);
        let call = req.call.as_ref().expect("call present");
        assert_eq!(call.method(), Some("run"));
        assert_eq!(call.args().unwrap()["arg"], 42);
    }

    #[test]
    fn launch_request_rejects_unknown_fields() {
        let json = serde_json::json!({
            "item_ref": "directive:x",
            "ref_bindings": {},
            "project_path": "/tmp/p",
            "parameters": {},
            "execution_policy": local_live_policy(),
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
    fn launch_request_routing_fields_are_derived_after_deserialization() {
        let json = serde_json::json!({
            "item_ref": "directive:x",
            "ref_bindings": {},
            "project_path": "/tmp/p",
            "parameters": {},
            "execution_policy": local_live_policy()
        });
        let req: LaunchRequest = serde_json::from_value(json).unwrap();
        assert!(req.launch_mode.is_empty());
        assert_eq!(req.target_site_id, None);
        assert!(!req.validate_only);
        assert!(req.call.is_none());
    }

    #[test]
    fn planning_envelope_exposes_only_the_opaque_launch_id() {
        let envelope = execution_planning_envelope("L-opaque");
        assert_eq!(envelope.event_type, "execution_planning");
        assert_eq!(
            envelope.payload,
            serde_json::json!({"launch_id": "L-opaque"})
        );
        assert!(envelope.payload.get("thread_id").is_none());
        assert!(envelope.payload.get("request_trace_id").is_none());
    }

    #[test]
    fn planning_capacity_maps_to_typed_503_without_launch_or_owner_details() {
        let error = map_launch_planning_reservation_error(
            ryeos_app::state_store::LaunchPlanningReservationError::CapacityExceeded(
                ryeos_app::state_store::LaunchPlanningCapacityExceeded,
            ),
        );
        let RouteDispatchError::ServiceUnavailable { code, message } = error else {
            panic!("planning capacity must remain a typed unavailable response");
        };
        assert_eq!(code, "launch_planning_capacity_exceeded");
        assert_eq!(
            message,
            "launch planning capacity is temporarily unavailable"
        );
        assert!(!message.contains("L-"));
        assert!(!message.contains("T-"));
        assert!(!message.contains("fp:"));
    }

    #[test]
    fn planning_reservation_storage_failure_remains_internal() {
        let error = map_launch_planning_reservation_error(
            ryeos_app::state_store::LaunchPlanningReservationError::Internal(anyhow::anyhow!(
                "runtime database unavailable"
            )),
        );
        assert!(matches!(error, RouteDispatchError::Internal(message)
            if message.contains("runtime database unavailable")));
    }

    #[tokio::test]
    async fn completed_launch_with_buffered_handoff_survives_delayed_first_poll() {
        let (sender, ready) = tokio::sync::oneshot::channel();
        sender.send(Ok("T-ready".to_string())).unwrap();
        let mut launch_handle = tokio::spawn(async { Ok(()) });
        while !launch_handle.is_finished() {
            tokio::task::yield_now().await;
        }

        let result = await_launch_handoff(&mut launch_handle, ready).await;
        assert!(matches!(result, Ok(thread_id) if thread_id == "T-ready"));
    }

    #[tokio::test]
    async fn structured_handoff_failure_is_the_first_error_envelope() {
        let (sender, ready) = tokio::sync::oneshot::channel();
        sender
            .send(Err(
                ryeos_executor::execution::launch::LaunchHandoffFailure {
                    code: "launch_preparation_failed".to_string(),
                    message: "preparation failed".to_string(),
                    status: 502,
                    body: serde_json::json!({
                        "code": "ignored_duplicate",
                        "error": "ignored duplicate",
                        "retryable": true,
                        "classification": "environment",
                    }),
                },
            ))
            .unwrap();
        let mut launch_handle = tokio::spawn(std::future::pending::<
            Result<(), crate::routes::launch::LaunchSpawnError>,
        >());

        let result = await_launch_handoff(&mut launch_handle, ready).await;
        let Err(envelope) = result else {
            panic!("structured handoff failure must be reported");
        };
        assert_eq!(envelope.event_type, "stream_error");
        assert_eq!(envelope.payload["code"], "launch_preparation_failed");
        assert_eq!(envelope.payload["error"], "preparation failed");
        assert_eq!(envelope.payload["retryable"], true);
        assert_eq!(envelope.payload["classification"], "environment");
        launch_handle.abort();
    }

    #[tokio::test]
    async fn launch_task_panic_before_handoff_is_a_launch_task_error() {
        let (sender, ready) = tokio::sync::oneshot::channel();
        let mut launch_handle = tokio::spawn(async move {
            let _sender = sender;
            panic!("synthetic launch panic");
            #[allow(unreachable_code)]
            Ok::<(), crate::routes::launch::LaunchSpawnError>(())
        });

        let result = await_launch_handoff(&mut launch_handle, ready).await;
        let Err(envelope) = result else {
            panic!("launch task panic must be reported");
        };
        assert_eq!(envelope.event_type, "stream_error");
        assert_eq!(envelope.payload["code"], "launch_task_failed");
        assert!(envelope.payload["error"]
            .as_str()
            .is_some_and(|message| message.contains("synthetic launch panic")));
    }

    #[test]
    fn handoff_identity_mismatch_has_a_stable_error_envelope() {
        assert!(launch_handoff_identity_error("T-expected", "T-expected").is_none());
        let envelope = launch_handoff_identity_error("T-expected", "T-other")
            .expect("mismatched handoff must fail");
        assert_eq!(envelope.event_type, "stream_error");
        assert_eq!(envelope.payload["code"], "launch_handoff_identity_mismatch");
        assert_eq!(
            envelope.payload["error"],
            "authoritative handoff returned a different thread identity"
        );
    }

    #[tokio::test]
    async fn closed_handoff_reports_completed_launch_failure_as_first_error() {
        let (sender, ready) = tokio::sync::oneshot::channel();
        drop(sender);
        let mut launch_handle = tokio::spawn(async {
            Err(crate::routes::launch::LaunchSpawnError::InvalidRef {
                ref_str: "bad ref".to_string(),
                reason: "invalid".to_string(),
            })
        });

        let result = await_launch_handoff(&mut launch_handle, ready).await;
        let Err(envelope) = result else {
            panic!("closed handoff must report launch failure");
        };
        assert_eq!(envelope.event_type, "stream_error");
        assert_eq!(envelope.payload["code"], "invalid_ref");
        assert_eq!(
            envelope.payload["error"],
            "invalid item_ref 'bad ref': invalid"
        );
    }

    #[test]
    fn abort_registry_capacity_has_a_stable_stream_error_envelope() {
        let envelope = completed_launch_error_envelope(Ok(Err(
            crate::routes::launch::LaunchSpawnError::AbortRegistryCapacityExceeded,
        )));

        assert_eq!(envelope.event_type, "stream_error");
        assert_eq!(
            envelope.payload["code"],
            "launch_abort_registry_capacity_exceeded"
        );
        assert_eq!(
            envelope.payload["error"],
            "active launch task signal registry reached its bounded capacity"
        );
    }

    #[test]
    fn completed_dispatch_error_retains_its_structured_envelope() {
        let envelope = completed_launch_error_envelope(Ok(Err(
            crate::routes::launch::LaunchSpawnError::Dispatch(
                ryeos_executor::dispatch_error::DispatchError::LaunchCancelled {
                    stage: "authoritative thread publication",
                },
            ),
        )));

        assert_eq!(envelope.event_type, "stream_error");
        assert_eq!(envelope.payload["code"], "launch_cancelled");
        assert_eq!(
            envelope.payload["error"],
            "launch was cancelled before authoritative thread publication"
        );
        assert_eq!(envelope.payload["retryable"], false);
        assert!(envelope.payload.get("thread_id").is_none());
    }

    #[tokio::test]
    async fn request_scoped_guard_aborts_but_daemon_owned_guard_does_not() {
        let request_task = tokio::spawn(std::future::pending::<()>());
        let request_guard = RequestScopedLaunchGuard(Some(request_task.abort_handle()));
        drop(request_guard);
        assert!(request_task.await.unwrap_err().is_cancelled());

        let (sender, receiver) = tokio::sync::oneshot::channel();
        let daemon_task = tokio::spawn(async move {
            tokio::task::yield_now().await;
            sender.send(42).unwrap();
        });
        let daemon_guard = RequestScopedLaunchGuard(None);
        drop(daemon_guard);
        drop(daemon_task);
        assert_eq!(receiver.await.unwrap(), 42);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dropping_unpolled_stream_drops_captured_request_launch_guard() {
        let request_task = tokio::spawn(std::future::pending::<()>());
        let launch_scope = RequestScopedLaunchGuard(Some(request_task.abort_handle()));
        let stream = async_stream::stream! {
            let _launch_scope = launch_scope;
            yield 1u8;
        };

        // On a current-thread runtime the task and stream are both unpolled.
        // Dropping the returned stream must still drop its captured ownership
        // guard and abort request-scoped launch work before handoff.
        drop(stream);
        assert!(request_task
            .await
            .expect_err("stream drop must abort request-scoped launch")
            .is_cancelled());
    }
}
