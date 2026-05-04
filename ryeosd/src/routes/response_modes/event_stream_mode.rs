//! `event_stream` response mode — unified SSE gateway + subscription.
//!
//! Two strategies, selected at compile time by the `source` field:
//!
//! | Source value        | Strategy     | Description                                  |
//! |---------------------|-------------|----------------------------------------------|
//! | `"dispatch_launch"` | Gateway     | Body-driven: parses item_ref from request body, mints thread ID, dispatches via engine, streams events |
//! | `"thread_events"`   | Subscription| Path-driven: extracts thread_id from path capture, subscribes to existing thread, streams events with principal ownership check |
//!
//! Both strategies produce SSE frames. Both share lag-recovery and
//! post-launch drain logic. The compile-time bifurcation ensures each
//! strategy gets its own validation rules.

use std::sync::Arc;
use std::time::Duration;

use axum::response::sse::{KeepAlive, Sse};
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::Value;

use crate::dispatch_error::{RouteConfigError, RouteDispatchError};
use crate::routes::compile::{
    CompiledResponseMode, CompiledRoute, ResponseMode, RouteDispatchContext,
};
use crate::routes::raw::{RawRequestBody, RawRouteSpec};
use crate::services::event_store::EventReplayParams;
use crate::state_store::PersistedEventRecord;

// ── Shared constants ────────────────────────────────────────────────────

/// Number of events to replay per batch during lag recovery.
const REPLAY_BATCH_SIZE: usize = 200;

const TERMINAL_EVENT_TYPES: &[&str] = &[
    "thread_completed",
    "thread_failed",
    "thread_cancelled",
    "thread_killed",
    "thread_timed_out",
];

fn is_terminal(event_type: &str) -> bool {
    TERMINAL_EVENT_TYPES.contains(&event_type)
}

fn is_terminal_status(status: &str) -> bool {
    matches!(
        status,
        "completed" | "failed" | "cancelled" | "killed" | "timed_out"
    )
}

fn sse_error_event(code: &str, message: &str) -> axum::response::sse::Event {
    axum::response::sse::Event::default()
        .event("stream_error")
        .data(
            serde_json::json!({"code": code, "error": message}).to_string(),
        )
}

fn sse_event_for_persisted(ev: &PersistedEventRecord) -> axum::response::sse::Event {
    axum::response::sse::Event::default()
        .event(ev.event_type.clone())
        .id(ev.chain_seq.to_string())
        .data(serde_json::to_string(ev).expect("PersistedEventRecord serializes"))
}

// ── Compile-time strategy selection ─────────────────────────────────────

/// The compiled strategy selected at route-table build time.
enum EventStreamStrategy {
    /// Body-driven launch: parse item_ref from request body, mint thread,
    /// dispatch via engine, tail events.
    Gateway {
        route_id: String,
        keep_alive_secs: u64,
    },
    /// Path-driven subscription: extract thread_id from path capture,
    /// check principal ownership, subscribe to existing thread.
    Subscription {
        thread_id_capture: String,
        keep_alive_secs: u64,
    },
}

pub struct EventStreamMode;

pub struct CompiledEventStreamMode {
    strategy: EventStreamStrategy,
}

impl ResponseMode for EventStreamMode {
    fn key(&self) -> &'static str {
        "event_stream"
    }

    fn allows_zero_timeout(&self) -> bool {
        true
    }

    fn compile(
        &self,
        raw: &RawRouteSpec,
    ) -> Result<Arc<dyn CompiledResponseMode>, RouteConfigError> {
        if raw.execute.is_some() {
            return Err(RouteConfigError::InvalidResponseSpec {
                id: raw.id.clone(),
                mode: "event_stream".into(),
                reason: "event_stream mode must not have a top-level 'execute' block".into(),
            });
        }

        let source = raw.response.source.as_deref().unwrap_or("");

        let strategy = match source {
            "dispatch_launch" => compile_gateway(raw)?,
            "thread_events" => compile_subscription(raw)?,
            "" => {
                return Err(RouteConfigError::InvalidResponseSpec {
                    id: raw.id.clone(),
                    mode: "event_stream".into(),
                    reason: "event_stream mode requires `response.source` \
                        (expected 'dispatch_launch' or 'thread_events')"
                        .into(),
                });
            }
            other => {
                return Err(RouteConfigError::InvalidSourceConfig {
                    id: raw.id.clone(),
                    src: other.into(),
                    reason: format!(
                        "unknown event_stream source '{other}'; \
                         expected 'dispatch_launch' or 'thread_events'"
                    ),
                });
            }
        };

        Ok(Arc::new(CompiledEventStreamMode { strategy }))
    }
}

// ── Gateway compile ─────────────────────────────────────────────────────

const GATEWAY_REQUIRED_AUTH: &str = "rye_signed";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawGatewaySourceConfig {
    keep_alive_secs: u64,
}

fn compile_gateway(raw: &RawRouteSpec) -> Result<EventStreamStrategy, RouteConfigError> {
    if raw.auth != GATEWAY_REQUIRED_AUTH {
        return Err(RouteConfigError::SourceAuthRequirement {
            id: raw.id.clone(),
            src: "dispatch_launch".into(),
            required: GATEWAY_REQUIRED_AUTH.into(),
            got: raw.auth.clone(),
        });
    }

    if raw.request.body != RawRequestBody::Json {
        return Err(RouteConfigError::InvalidSourceConfig {
            id: raw.id.clone(),
            src: "dispatch_launch".into(),
            reason: "dispatch_launch requires request.body = json".into(),
        });
    }

    let cfg: RawGatewaySourceConfig =
        serde_json::from_value(raw.response.source_config.clone()).map_err(|e| {
            RouteConfigError::InvalidSourceConfig {
                id: raw.id.clone(),
                src: "dispatch_launch".into(),
                reason: format!("invalid source_config: {e}"),
            }
        })?;

    if cfg.keep_alive_secs == 0 {
        return Err(RouteConfigError::InvalidSourceConfig {
            id: raw.id.clone(),
            src: "dispatch_launch".into(),
            reason: "keep_alive_secs must be > 0".into(),
        });
    }

    Ok(EventStreamStrategy::Gateway {
        route_id: raw.id.clone(),
        keep_alive_secs: cfg.keep_alive_secs,
    })
}

// ── Subscription compile ────────────────────────────────────────────────

const SUBSCRIPTION_REQUIRED_AUTH: &str = "rye_signed";

fn compile_subscription(raw: &RawRouteSpec) -> Result<EventStreamStrategy, RouteConfigError> {
    if raw.auth != SUBSCRIPTION_REQUIRED_AUTH {
        return Err(RouteConfigError::SourceAuthRequirement {
            id: raw.id.clone(),
            src: "thread_events".into(),
            required: SUBSCRIPTION_REQUIRED_AUTH.into(),
            got: raw.auth.clone(),
        });
    }

    let source_config = &raw.response.source_config;

    let thread_id_template = source_config
        .get("thread_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| RouteConfigError::InvalidSourceConfig {
            id: raw.id.clone(),
            src: "thread_events".into(),
            reason: "missing 'thread_id' in source_config".into(),
        })?;

    let capture_name = validate_and_extract_path_capture(
        thread_id_template,
        "thread_id",
        &raw.id,
    )?;

    let declared_captures = extract_path_captures(&raw.path);
    if !declared_captures.contains(&capture_name) {
        return Err(RouteConfigError::InvalidSourceConfig {
            id: raw.id.clone(),
            src: "thread_events".into(),
            reason: format!(
                "thread_id references undeclared path capture '{capture_name}'; \
                 route path declares: [{declared}]",
                declared = declared_captures
                    .iter()
                    .map(|c| format!("'{c}'"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        });
    }

    let keep_alive_secs = source_config
        .get("keep_alive_secs")
        .and_then(|v| v.as_u64())
        .unwrap_or(15);
    if keep_alive_secs == 0 {
        return Err(RouteConfigError::InvalidSourceConfig {
            id: raw.id.clone(),
            src: "thread_events".into(),
            reason: "keep_alive_secs must be > 0".into(),
        });
    }

    Ok(EventStreamStrategy::Subscription {
        thread_id_capture: capture_name,
        keep_alive_secs,
    })
}

/// Validate that `template` is a single `${path.<name>}` interpolation
/// and return the capture name.
fn validate_and_extract_path_capture(
    template: &str,
    field: &str,
    route_id: &str,
) -> Result<String, RouteConfigError> {
    let trimmed = template.trim();
    let prefix = "${path.";
    let suffix = "}";

    // Must contain ${...}
    if let Some(start) = trimmed.find("${") {
        if let Some(end_offset) = trimmed[start..].find('}') {
            let inner = &trimmed[start + 2..start + end_offset];
            if !inner.starts_with("path.") {
                return Err(RouteConfigError::InvalidSourceConfig {
                    id: route_id.into(),
                    src: "thread_events".into(),
                    reason: format!(
                        "{field} must use ${{path.<name>}} interpolation, got ${{{inner}}}"
                    ),
                });
            }
        } else {
            return Err(RouteConfigError::InvalidSourceConfig {
                id: route_id.into(),
                src: "thread_events".into(),
                reason: format!("{field} contains unterminated '${{...}}' template"),
            });
        }

        // Must be a single template (no second interpolation).
        let after_first = &trimmed[start + 2..];
        if after_first.find("${").is_some() {
            return Err(RouteConfigError::InvalidSourceConfig {
                id: route_id.into(),
                src: "thread_events".into(),
                reason: format!("{field} must be a single ${{path.<name>}} template"),
            });
        }
    } else {
        return Err(RouteConfigError::InvalidSourceConfig {
            id: route_id.into(),
            src: "thread_events".into(),
            reason: format!("{field} must use ${{path.<name>}} interpolation"),
        });
    }

    // Extract the capture name.
    if let Some(rest) = trimmed.strip_prefix(prefix) {
        if let Some(name) = rest.strip_suffix(suffix) {
            return Ok(name.to_string());
        }
    }

    Err(RouteConfigError::InvalidSourceConfig {
        id: route_id.into(),
        src: "thread_events".into(),
        reason: format!("{field} has invalid path capture template"),
    })
}

fn extract_path_captures(path: &str) -> std::collections::HashSet<String> {
    let mut captures = std::collections::HashSet::new();
    for segment in path.split('/').skip(1) {
        if let Some(name) = segment.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
            captures.insert(name.to_string());
        }
    }
    captures
}

// ── Dispatch (request-time) ─────────────────────────────────────────────

#[axum::async_trait]
impl CompiledResponseMode for CompiledEventStreamMode {
    fn is_streaming(&self) -> bool {
        true
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn handle(
        &self,
        _compiled: &CompiledRoute,
        ctx: RouteDispatchContext,
    ) -> Result<axum::response::Response, RouteDispatchError> {
        let last_event_id = parse_last_event_id(&ctx.request_parts.headers)?;

        let sse_result = match &self.strategy {
            EventStreamStrategy::Gateway {
                route_id,
                keep_alive_secs,
            } => dispatch_gateway(&ctx, route_id, *keep_alive_secs).await?,
            EventStreamStrategy::Subscription {
                thread_id_capture,
                keep_alive_secs,
            } => dispatch_subscription(&ctx, thread_id_capture, last_event_id, *keep_alive_secs)
                .await?,
        };

        let body_stream = sse_result.stream;
        let keep_alive_secs = sse_result.keep_alive_secs.max(1);

        let keep_alive = KeepAlive::new()
            .interval(Duration::from_secs(keep_alive_secs))
            .text(":");

        let sse = Sse::new(body_stream).keep_alive(keep_alive);

        Ok(sse.into_response())
    }
}

/// SSE event stream returned by both strategies.
struct SseEventStream {
    stream: std::pin::Pin<
        Box<
            dyn tokio_stream::Stream<
                    Item = Result<axum::response::sse::Event, std::convert::Infallible>,
                > + Send,
        >,
    >,
    keep_alive_secs: u64,
}

// ── Gateway dispatch ────────────────────────────────────────────────────

/// Typed body shape for `POST /execute/stream`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LaunchRequest {
    item_ref: String,
    project_path: String,
    parameters: Value,
}

async fn dispatch_gateway(
    ctx: &RouteDispatchContext,
    route_id: &str,
    keep_alive_secs: u64,
) -> Result<SseEventStream, RouteDispatchError> {
    // Gateway does not support Last-Event-ID (it mints a new thread).
    // The caller must start from the beginning of the new thread's events.

    let req: LaunchRequest = serde_json::from_slice(&ctx.body_raw).map_err(|e| {
        RouteDispatchError::BadRequest(format!("invalid request body: {e}"))
    })?;

    let item_ref = crate::routes::parsed_ref::ParsedItemRef::parse(&req.item_ref).map_err(|e| {
        RouteDispatchError::BadRequest(format!(
            "invalid item_ref '{}': {}",
            req.item_ref, e
        ))
    })?;

    let project_path =
        crate::routes::abs_path::AbsolutePathBuf::try_from_str(&req.project_path).map_err(
            |e| RouteDispatchError::BadRequest(format!("project_path: {e}")),
        )?;

    let thread_id = crate::services::thread_lifecycle::new_thread_id();

    let span = tracing::info_span!(
        "dispatch_launch_sse",
        route_id = route_id,
        thread_id = thread_id.as_str(),
        item_ref_kind = item_ref.kind(),
    );

    let hub = ctx.state.event_streams.clone();
    let mut rx = hub.subscribe(&thread_id);

    let mut launch_handle = crate::routes::launch::spawn_dispatch_launch(
        &ctx.state,
        item_ref,
        project_path,
        req.parameters,
        ctx.principal.id.clone(),
        ctx.principal.scopes.clone(),
        thread_id.clone(),
    );

    let events_svc = ctx.state.events.clone();
    let state_store_clone = ctx.state.state_store.clone();
    let thread_id_for_stream = thread_id.clone();

    let stream = async_stream::stream! {
        let _guard = span.enter();
        yield Ok(
            axum::response::sse::Event::default()
                .event("stream_started")
                .data(serde_json::json!({"thread_id": thread_id_for_stream}).to_string())
        );

        let mut current_max: i64 = 0;
        let replay_batch_size = REPLAY_BATCH_SIZE;

        loop {
            tokio::select! {
                recv_result = rx.recv() => {
                    match recv_result {
                        Ok(ev) => {
                            let event_type = ev.event_type.clone();
                            if ev.chain_seq > current_max {
                                current_max = ev.chain_seq;
                                yield Ok(
                                    axum::response::sse::Event::default()
                                        .event(ev.event_type.clone())
                                        .id(ev.chain_seq.to_string())
                                        .data(serde_json::to_string(&ev).expect("PersistedEventRecord serializes"))
                                );
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
                                                yield Ok(
                                                    axum::response::sse::Event::default()
                                                        .event(ev.event_type.clone())
                                                        .id(ev.chain_seq.to_string())
                                                        .data(serde_json::to_string(&ev).expect("PersistedEventRecord serializes"))
                                                );
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
                                yield Ok(sse_error_event("replay_failed", &err_msg));
                                return;
                            }

                            current_max = lag_max;

                            tracing::info!(
                                thread_id = %thread_id_for_stream,
                                lagged = n,
                                "dispatch_launch SSE subscriber lag recovery complete"
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
                                                yield Ok(
                                                    axum::response::sse::Event::default()
                                                        .event(ev.event_type.clone())
                                                        .id(ev.chain_seq.to_string())
                                                        .data(serde_json::to_string(&ev).expect("PersistedEventRecord serializes"))
                                                );
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
                                        yield Ok(sse_error_event("post_launch_replay_failed", &format!("post-launch replay failed: {e}")));
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
                            yield Ok(sse_error_event("thread_not_terminal", "launch completed but thread is not terminal"));
                            return;
                        }
                        Ok(Err(e)) => {
                            yield Ok(sse_error_event(e.code(), &format!("launch failed: {e}")));
                            return;
                        }
                        Err(_) => {
                            yield Ok(sse_error_event("task_panicked", "launch task panicked"));
                            return;
                        }
                    }
                }
            }
        }
    };

    Ok(SseEventStream {
        stream: Box::pin(stream),
        keep_alive_secs,
    })
}

// ── Subscription dispatch ───────────────────────────────────────────────

async fn dispatch_subscription(
    ctx: &RouteDispatchContext,
    thread_id_capture: &str,
    last_event_id: Option<i64>,
    keep_alive_secs: u64,
) -> Result<SseEventStream, RouteDispatchError> {
    let thread_id = ctx
        .captures
        .get(thread_id_capture)
        .ok_or_else(|| {
            RouteDispatchError::Internal(format!(
                "path capture '{}' not found in request",
                thread_id_capture
            ))
        })?
        .clone();

    let thread_detail = ctx
        .state
        .state_store
        .get_thread(&thread_id)
        .map_err(|e| RouteDispatchError::Internal(e.to_string()))?
        .ok_or(RouteDispatchError::NotFound)?;

    // Principal ownership check: only the thread creator can subscribe.
    let principal_id = &ctx.principal.id;
    let requested_by = match &thread_detail.requested_by {
        Some(r) => r,
        None => return Err(RouteDispatchError::NotFound),
    };

    if principal_id != requested_by {
        return Err(RouteDispatchError::NotFound);
    }

    let hub = ctx.state.event_streams.clone();
    let mut rx = hub.subscribe(&thread_id);

    let last_seen = last_event_id;

    let events_svc = ctx.state.events.clone();
    let state_store_clone = ctx.state.state_store.clone();

    let stream = async_stream::stream! {
        yield Ok(
            axum::response::sse::Event::default()
                .event("stream_started")
                .data(serde_json::json!({"thread_id": thread_id}).to_string())
        );

        let replay_batch_size = REPLAY_BATCH_SIZE;

        let replay_result = events_svc
            .replay(&EventReplayParams {
                chain_root_id: None,
                thread_id: Some(thread_id.clone()),
                after_chain_seq: last_seen,
                limit: replay_batch_size,
            })
            .map_err(|e| RouteDispatchError::Internal(e.to_string()));

        match replay_result {
            Ok(result) => {
                let mut max_seq = last_seen.unwrap_or(0);
                let mut saw_terminal = false;
                let mut yielded_any_replay_event = false;

                for ev in &result.events {
                    max_seq = max_seq.max(ev.chain_seq);
                    yield Ok(sse_event_for_persisted(ev));
                    yielded_any_replay_event = true;
                    if is_terminal(&ev.event_type) {
                        saw_terminal = true;
                    }
                }

                if saw_terminal {
                    return;
                }

                let mut cursor = result.next_cursor;
                while let Some(_c) = cursor {
                    let page = events_svc
                        .replay(&EventReplayParams {
                            chain_root_id: None,
                            thread_id: Some(thread_id.clone()),
                            after_chain_seq: Some(max_seq),
                            limit: replay_batch_size,
                        });

                    match page {
                        Ok(page_result) => {
                            if page_result.events.is_empty() {
                                break;
                            }
                            for ev in &page_result.events {
                                max_seq = max_seq.max(ev.chain_seq);
                                yield Ok(sse_event_for_persisted(ev));
                                yielded_any_replay_event = true;
                                if is_terminal(&ev.event_type) {
                                    return;
                                }
                            }
                            cursor = page_result.next_cursor;
                        }
                        Err(e) => {
                            yield Ok(sse_error_event("replay_paging_failed", &format!("replay paging failed: {e}")));
                            return;
                        }
                    }
                }

                if !yielded_any_replay_event {
                    let detail = state_store_clone.get_thread(&thread_id);
                    if let Ok(Some(d)) = &detail {
                        if is_terminal_status(&d.status) {
                            return;
                        }
                    }
                }

                let mut current_max = max_seq;
                loop {
                    match rx.recv().await {
                        Ok(ev) => {
                            if ev.chain_seq <= current_max {
                                continue;
                            }
                            current_max = ev.chain_seq;
                            yield Ok(sse_event_for_persisted(&ev));
                            if is_terminal(&ev.event_type) {
                                return;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            let lag_cursor = current_max;
                            let mut lag_max = current_max;
                            let mut lag_error: Option<String> = None;

                            loop {
                                let page_result = events_svc
                                    .replay(&EventReplayParams {
                                        chain_root_id: None,
                                        thread_id: Some(thread_id.clone()),
                                        after_chain_seq: if lag_max > lag_cursor {
                                            Some(lag_max)
                                        } else {
                                            Some(lag_cursor)
                                        },
                                        limit: replay_batch_size,
                                    });

                                match page_result {
                                    Ok(page) => {
                                        if page.events.is_empty() {
                                            break;
                                        }
                                        for ev in &page.events {
                                            lag_max = lag_max.max(ev.chain_seq);
                                            if ev.chain_seq > current_max {
                                                yield Ok(sse_event_for_persisted(ev));
                                                if is_terminal(&ev.event_type) {
                                                    return;
                                                }
                                            }
                                        }
                                        if page.next_cursor.is_none() {
                                            break;
                                        }
                                    }
                                    Err(e) => {
                                        lag_error = Some(format!("replay failed: {e}"));
                                        break;
                                    }
                                }
                            }

                            if let Some(err_msg) = lag_error {
                                let thread = state_store_clone.get_thread(&thread_id);
                                if let Ok(Some(detail)) = thread {
                                    if is_terminal_status(&detail.status) {
                                        return;
                                    }
                                }
                                yield Ok(sse_error_event("replay_failed", &err_msg));
                                return;
                            }

                            current_max = lag_max;

                            tracing::info!(
                                thread_id = %thread_id,
                                lagged = n,
                                "SSE subscriber lag recovery complete"
                            );
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            return;
                        }
                    }
                }
            }
            Err(e) => {
                yield Ok(sse_error_event("initial_replay_failed", &format!("replay failed: {e}")));
                return;
            }
        }
    };

    Ok(SseEventStream {
        stream: Box::pin(stream),
        keep_alive_secs,
    })
}

// ── Last-Event-ID parsing ───────────────────────────────────────────────

fn parse_last_event_id(
    headers: &axum::http::HeaderMap,
) -> Result<Option<i64>, RouteDispatchError> {
    let raw = match headers.get("last-event-id") {
        Some(v) => v,
        None => return Ok(None),
    };

    let s = raw.to_str().map_err(|_| RouteDispatchError::BadLastEventId)?;
    let n = s
        .parse::<i64>()
        .map_err(|_| RouteDispatchError::BadLastEventId)?;

    if n < 0 {
        return Err(RouteDispatchError::BadLastEventId);
    }

    Ok(Some(n))
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::raw::{RawLimits, RawRequest, RawRequestBody, RawResponseSpec};

    fn compile_ctx() {}

    // ── Last-Event-ID tests ────────────────────

    #[test]
    fn parse_valid_last_event_id() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("last-event-id", "42".parse().unwrap());
        assert_eq!(parse_last_event_id(&headers).unwrap(), Some(42));
    }

    #[test]
    fn parse_missing_last_event_id() {
        let headers = axum::http::HeaderMap::new();
        assert_eq!(parse_last_event_id(&headers).unwrap(), None);
    }

    #[test]
    fn parse_non_numeric_last_event_id_rejected() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("last-event-id", "abc".parse().unwrap());
        assert!(matches!(
            parse_last_event_id(&headers),
            Err(RouteDispatchError::BadLastEventId)
        ));
    }

    #[test]
    fn parse_empty_last_event_id_rejected() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("last-event-id", "".parse().unwrap());
        assert!(matches!(
            parse_last_event_id(&headers),
            Err(RouteDispatchError::BadLastEventId)
        ));
    }

    #[test]
    fn parse_negative_last_event_id_rejected() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("last-event-id", "-1".parse().unwrap());
        assert!(matches!(
            parse_last_event_id(&headers),
            Err(RouteDispatchError::BadLastEventId)
        ));
    }

    #[test]
    fn allows_zero_timeout() {
        assert!(EventStreamMode.allows_zero_timeout());
    }

    // ── Compile-time validation ────────────────

    fn make_gateway_raw(id: &str, path: &str) -> RawRouteSpec {
        RawRouteSpec {
            section: "routes".into(),
            id: id.into(),
            path: path.into(),
            methods: ["POST".into()].into_iter().collect(),
            auth: "rye_signed".into(),
            auth_config: None,
            limits: RawLimits::default(),
            response: RawResponseSpec {
                mode: "event_stream".into(),
                source: Some("dispatch_launch".into()),
                source_config: serde_json::json!({
                    "keep_alive_secs": 15,
                }),
                status: None,
                content_type: None,
                body_b64: None,
            },
            execute: None,
            request: RawRequest {
                body: RawRequestBody::Json,
            },
            source_file: std::path::PathBuf::from(format!("/test/{id}.yaml")),
        }
    }

    fn make_subscription_raw(id: &str, path: &str) -> RawRouteSpec {
        RawRouteSpec {
            section: "routes".into(),
            id: id.into(),
            path: path.into(),
            methods: ["GET".into()].into_iter().collect(),
            auth: "rye_signed".into(),
            auth_config: None,
            limits: RawLimits::default(),
            response: RawResponseSpec {
                mode: "event_stream".into(),
                source: Some("thread_events".into()),
                source_config: serde_json::json!({
                    "thread_id": "${path.id}",
                    "keep_alive_secs": 15,
                }),
                status: None,
                content_type: None,
                body_b64: None,
            },
            execute: None,
            request: RawRequest {
                body: RawRequestBody::None,
            },
            source_file: std::path::PathBuf::from(format!("/test/{id}.yaml")),
        }
    }

    #[test]
    fn compile_gateway_succeeds() {
        let raw = make_gateway_raw("r1", "/execute/stream");
        let result = EventStreamMode.compile(&raw);
        assert!(result.is_ok(), "gateway compile should succeed");
    }

    #[test]
    fn compile_subscription_succeeds() {
        let raw = make_subscription_raw("r1", "/threads/{id}/stream");
        let result = EventStreamMode.compile(&raw);
        assert!(result.is_ok(), "subscription compile should succeed");
    }

    #[test]
    fn compile_rejects_execute_block() {
        use crate::routes::raw::RawExecute;
        let mut raw = make_gateway_raw("r1", "/execute/stream");
        raw.execute = Some(RawExecute {
            item_ref: "tool:x/y".into(),
            params: serde_json::Value::Null,
        });
        let result = EventStreamMode.compile(&raw);
        let err = match result {
            Err(e) => e,
            Ok(_) => panic!("expected error"),
        };
        let msg = format!("{err}");
        assert!(
            msg.contains("must not have a top-level 'execute' block"),
            "got: {msg}"
        );
    }

    #[test]
    fn compile_rejects_unknown_source() {
        let mut raw = make_gateway_raw("r1", "/test/{id}");
        raw.response.source = Some("nonexistent_source".into());
        raw.response.source_config = serde_json::Value::Null;
        let result = EventStreamMode.compile(&raw);
        let err = match result {
            Err(e) => e,
            Ok(_) => panic!("expected error"),
        };
        let msg = format!("{err}");
        assert!(
            msg.contains("unknown event_stream source"),
            "got: {msg}"
        );
    }

    #[test]
    fn compile_rejects_missing_source() {
        let mut raw = make_gateway_raw("r1", "/test/{id}");
        raw.response.source = None;
        raw.response.source_config = serde_json::Value::Null;
        let result = EventStreamMode.compile(&raw);
        let err = match result {
            Err(e) => e,
            Ok(_) => panic!("expected error"),
        };
        let msg = format!("{err}");
        assert!(
            msg.contains("requires `response.source`"),
            "got: {msg}"
        );
    }

    // ── Gateway-specific compile tests ─────────

    #[test]
    fn gateway_rejects_auth_none() {
        let mut raw = make_gateway_raw("r1", "/execute/stream");
        raw.auth = "none".into();
        let result = EventStreamMode.compile(&raw);
        let err = match result { Err(e) => e, Ok(_) => panic!("expected error") };
        let msg = format!("{err}");
        assert!(msg.contains("requires auth 'rye_signed'"), "got: {msg}");
    }

    #[test]
    fn gateway_rejects_non_json_body() {
        let mut raw = make_gateway_raw("r1", "/execute/stream");
        raw.request.body = RawRequestBody::Raw;
        let result = EventStreamMode.compile(&raw);
        let err = match result { Err(e) => e, Ok(_) => panic!("expected error") };
        let msg = format!("{err}");
        assert!(msg.contains("requires request.body = json"), "got: {msg}");
    }

    #[test]
    fn gateway_rejects_keep_alive_zero() {
        let mut raw = make_gateway_raw("r1", "/execute/stream");
        raw.response.source_config = serde_json::json!({"keep_alive_secs": 0});
        let result = EventStreamMode.compile(&raw);
        let err = match result { Err(e) => e, Ok(_) => panic!("expected error") };
        let msg = format!("{err}");
        assert!(msg.contains("keep_alive_secs must be > 0"), "got: {msg}");
    }

    #[test]
    fn gateway_rejects_unknown_source_config_key() {
        let mut raw = make_gateway_raw("r1", "/execute/stream");
        raw.response.source_config = serde_json::json!({
            "keep_alive_secs": 15,
            "item_ref": "bogus",
        });
        let result = EventStreamMode.compile(&raw);
        let err = match result { Err(e) => e, Ok(_) => panic!("expected error") };
        let msg = format!("{err}");
        assert!(
            msg.contains("unknown field") && msg.contains("item_ref"),
            "got: {msg}"
        );
    }

    #[test]
    fn gateway_rejects_missing_keep_alive_secs() {
        let mut raw = make_gateway_raw("r1", "/execute/stream");
        raw.response.source_config = serde_json::json!({});
        let result = EventStreamMode.compile(&raw);
        let err = match result { Err(e) => e, Ok(_) => panic!("expected error") };
        let msg = format!("{err}");
        assert!(
            msg.contains("missing field `keep_alive_secs`"),
            "got: {msg}"
        );
    }

    #[test]
    fn gateway_rejects_non_object_source_config() {
        let mut raw = make_gateway_raw("r1", "/execute/stream");
        raw.response.source_config = serde_json::json!(123);
        let result = EventStreamMode.compile(&raw);
        let err = match result { Err(e) => e, Ok(_) => panic!("expected error") };
        let msg = format!("{err}");
        assert!(msg.contains("invalid source_config"), "got: {msg}");
    }

    #[test]
    fn launch_request_rejects_missing_fields() {
        let body = serde_json::json!({"item_ref": "directive:foo"});
        let bytes = serde_json::to_vec(&body).unwrap();
        let err = serde_json::from_slice::<LaunchRequest>(&bytes)
            .expect_err("must reject missing fields");
        let msg = err.to_string();
        assert!(msg.contains("missing field"), "expected missing-field error, got: {msg}");
    }

    #[test]
    fn launch_request_rejects_unknown_field() {
        let body = serde_json::json!({
            "item_ref": "directive:foo",
            "project_path": "/tmp",
            "parameters": {},
            "extra": "nope",
        });
        let bytes = serde_json::to_vec(&body).unwrap();
        let err = serde_json::from_slice::<LaunchRequest>(&bytes)
            .expect_err("must reject unknown fields");
        let msg = err.to_string();
        assert!(
            msg.contains("unknown field") && msg.contains("extra"),
            "expected unknown-field error, got: {msg}"
        );
    }

    #[test]
    fn launch_request_accepts_complete_body() {
        let body = serde_json::json!({
            "item_ref": "directive:my/agent",
            "project_path": "/tmp/proj",
            "parameters": {"name": "World"},
        });
        let bytes = serde_json::to_vec(&body).unwrap();
        let req: LaunchRequest =
            serde_json::from_slice(&bytes).expect("valid body must parse");
        assert_eq!(req.item_ref, "directive:my/agent");
        assert_eq!(req.project_path, "/tmp/proj");
        assert_eq!(req.parameters["name"], "World");
    }

    // ── Subscription-specific compile tests ────

    #[test]
    fn subscription_rejects_auth_none() {
        let mut raw = make_subscription_raw("r1", "/threads/{id}/stream");
        raw.auth = "none".into();
        let result = EventStreamMode.compile(&raw);
        let err = match result { Err(e) => e, Ok(_) => panic!("expected error") };
        let msg = format!("{err}");
        assert!(
            msg.contains("requires auth 'rye_signed'"),
            "got: {msg}"
        );
    }

    #[test]
    fn subscription_rejects_non_path_interpolation() {
        let mut raw = make_subscription_raw("r1", "/threads/{id}/stream");
        raw.response.source_config = serde_json::json!({
            "thread_id": "${query.id}",
            "keep_alive_secs": 15,
        });
        let result = EventStreamMode.compile(&raw);
        let err = match result { Err(e) => e, Ok(_) => panic!("expected error") };
        let msg = format!("{err}");
        assert!(msg.contains("must use ${path."), "got: {msg}");
    }

    #[test]
    fn subscription_rejects_undeclared_capture() {
        let mut raw = make_subscription_raw("r1", "/threads/{tid}/stream");
        raw.response.source_config = serde_json::json!({
            "thread_id": "${path.wrong}",
            "keep_alive_secs": 15,
        });
        let result = EventStreamMode.compile(&raw);
        let err = match result { Err(e) => e, Ok(_) => panic!("expected error") };
        let msg = format!("{err}");
        assert!(msg.contains("undeclared path capture"), "got: {msg}");
    }

    #[test]
    fn subscription_rejects_keep_alive_zero() {
        let mut raw = make_subscription_raw("r1", "/threads/{id}/stream");
        raw.response.source_config = serde_json::json!({
            "thread_id": "${path.id}",
            "keep_alive_secs": 0,
        });
        let result = EventStreamMode.compile(&raw);
        let err = match result { Err(e) => e, Ok(_) => panic!("expected error") };
        let msg = format!("{err}");
        assert!(msg.contains("keep_alive_secs must be > 0"), "got: {msg}");
    }

    // ── Shared helpers ─────────────────────────

    #[test]
    fn is_terminal_status_detects_all() {
        assert!(is_terminal_status("completed"));
        assert!(is_terminal_status("failed"));
        assert!(is_terminal_status("cancelled"));
        assert!(is_terminal_status("killed"));
        assert!(is_terminal_status("timed_out"));
        assert!(!is_terminal_status("running"));
        assert!(!is_terminal_status("pending"));
    }

    #[test]
    fn extract_path_capture_valid() {
        assert_eq!(
            validate_and_extract_path_capture("${path.thread_id}", "thread_id", "r1").unwrap(),
            "thread_id"
        );
    }

    #[test]
    fn extract_path_capture_rejects_non_path() {
        assert!(validate_and_extract_path_capture("${query.id}", "thread_id", "r1").is_err());
    }

    #[test]
    fn extract_path_captures_finds_all() {
        let caps = extract_path_captures("/threads/{id}/events/{sub}");
        assert!(caps.contains("id"));
        assert!(caps.contains("sub"));
        assert_eq!(caps.len(), 2);
    }

    #[test]
    fn validate_rejects_double_interpolation() {
        assert!(
            validate_and_extract_path_capture("${path.x}-${path.y}", "f", "r1").is_err()
        );
    }

    #[test]
    fn validate_rejects_static_string() {
        assert!(
            validate_and_extract_path_capture("static", "f", "r1").is_err()
        );
    }
}
