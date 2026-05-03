//! `/execute/stream` SSE source — kind-agnostic streaming gateway.
//!
//! This source mints a thread id, dispatches the launch via the shared
//! `spawn_dispatch_launch` helper, and tails events for that thread
//! using the persistence-first `(broadcast + paged replay on lag)`
//! pattern.
//!
//! The source does NOT pattern-match on the kind name in `item_ref`.
//! Whether `<kind>:<id>` is root-executable is the engine's call,
//! made inside `dispatch::dispatch` against the kind-schema registry.
//! When the engine returns `DispatchError::NotRootExecutable`, the
//! launch helper surfaces it as a typed `LaunchSpawnError::Dispatch`
//! and this source emits a structured `stream_error` SSE event with
//! `code = "not_root_executable"`.
//!
//! Required auth: `rye_signed` (TCP /execute/stream is publicly bound).
//! Required body: the typed `LaunchRequest` (item_ref + project_path
//! + parameters), all three required, no extras (deny_unknown_fields).
//!
//! `source_config` is also typed: `keep_alive_secs` (required, > 0),
//! enforced with `deny_unknown_fields`. No silent defaults.

use std::sync::Arc;

use serde::Deserialize;
use serde_json::Value;

use crate::dispatch_error::{RouteConfigError, RouteDispatchError};
use crate::routes::compile::RouteDispatchContext;
use crate::routes::raw::RawRouteSpec;
use crate::routes::streaming_sources::{
    BoundStreamingSource, RawEventStreamResponse, SseEventStream, SourceCompileContext,
    StreamingSource,
};
use crate::services::event_store::EventReplayParams;
use crate::state::AppState;

pub struct DispatchLaunchSource;

const REQUIRED_AUTH: &str = "rye_signed";

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

fn sse_error_event(code: &str, message: &str) -> axum::response::sse::Event {
    axum::response::sse::Event::default()
        .event("stream_error")
        .data(
            serde_json::json!({"code": code, "error": message}).to_string(),
        )
}

fn is_terminal_status(status: &str) -> bool {
    matches!(
        status,
        "completed" | "failed" | "cancelled" | "killed" | "timed_out"
    )
}

/// Typed deserialization for the `source_config` object in a
/// `dispatch_launch` route's `response.source_config`.
///
/// Required fields only; `deny_unknown_fields` rejects any extra
/// keys at route-table load time.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDispatchLaunchSourceConfig {
    keep_alive_secs: u64,
}

/// Typed body shape for `POST /execute/stream` (and any other route
/// that wires a body-driven launch).
///
/// Three required fields, no others. The verifier (`rye_signed`)
/// commits to the body bytes, so a typed deserialization with
/// `deny_unknown_fields` is the source-of-truth contract for what
/// the SSE source consumes — no template indirection, no
/// `Option<...>.unwrap_or(Value::Null)` silent fallbacks.
///
/// `parameters: Value` is required (must be present in the body), but
/// its inner shape is up to the launched item's input schema.
/// `item_ref` is a canonical ref (`<kind>:<id>`); the engine's kind
/// schema decides whether the kind is root-executable. The daemon
/// does NOT pattern-match on the kind name.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LaunchRequest {
    item_ref: String,
    project_path: String,
    parameters: Value,
}

impl StreamingSource for DispatchLaunchSource {
    fn key(&self) -> &'static str {
        "dispatch_launch"
    }

    fn compile(
        &self,
        raw_route: &RawRouteSpec,
        raw_event_stream: &RawEventStreamResponse,
        ctx: &SourceCompileContext,
    ) -> Result<Arc<dyn BoundStreamingSource>, RouteConfigError> {
        if ctx.auth_verifier_key != REQUIRED_AUTH {
            return Err(RouteConfigError::SourceAuthRequirement {
                id: raw_route.id.clone(),
                src: "dispatch_launch".into(),
                required: REQUIRED_AUTH.into(),
                got: ctx.auth_verifier_key.into(),
            });
        }

        if raw_route.request.body != crate::routes::raw::RawRequestBody::Json {
            return Err(RouteConfigError::InvalidSourceConfig {
                id: raw_route.id.clone(),
                src: "dispatch_launch".into(),
                reason: "dispatch_launch requires request.body = json".into(),
            });
        }

        let cfg: RawDispatchLaunchSourceConfig = serde_json::from_value(
            raw_event_stream.source_config.clone(),
        )
        .map_err(|e| RouteConfigError::InvalidSourceConfig {
            id: raw_route.id.clone(),
            src: "dispatch_launch".into(),
            reason: format!("invalid source_config: {e}"),
        })?;
        if cfg.keep_alive_secs == 0 {
            return Err(RouteConfigError::InvalidSourceConfig {
                id: raw_route.id.clone(),
                src: "dispatch_launch".into(),
                reason: "keep_alive_secs must be > 0".into(),
            });
        }

        Ok(Arc::new(CompiledDispatchLaunchSource {
            route_id: raw_route.id.clone(),
            keep_alive_secs: cfg.keep_alive_secs,
        }))
    }
}

struct CompiledDispatchLaunchSource {
    route_id: String,
    keep_alive_secs: u64,
}

#[axum::async_trait]
impl BoundStreamingSource for CompiledDispatchLaunchSource {
    async fn open(
        &self,
        ctx: &RouteDispatchContext,
        last_event_id: Option<i64>,
        state: &AppState,
    ) -> Result<SseEventStream, RouteDispatchError> {
        if last_event_id.is_some() {
            return Err(RouteDispatchError::BadRequest(
                "dispatch_launch does not support Last-Event-ID".into(),
            ));
        }

        // Typed body deserialization: the three required fields
        // (item_ref, project_path, parameters) MUST be present, with
        // no unknown extras. Missing fields → 400.
        let req: LaunchRequest = serde_json::from_slice(&ctx.body_raw).map_err(|e| {
            RouteDispatchError::BadRequest(format!("invalid request body: {e}"))
        })?;

        // Syntactic ref validation only. The engine's kind-schema
        // registry decides which kinds are root-executable; if the
        // ref's kind isn't, `dispatch::dispatch` returns a typed
        // `NotRootExecutable` error which surfaces as a stream_error
        // event. The daemon does not pattern-match the kind name.
        let item_ref = crate::routes::parsed_ref::ParsedItemRef::parse(&req.item_ref).map_err(
            |e| {
                RouteDispatchError::BadRequest(format!(
                    "invalid item_ref '{}': {}",
                    req.item_ref, e
                ))
            },
        )?;

        let project_path =
            crate::routes::abs_path::AbsolutePathBuf::try_from_str(&req.project_path).map_err(
                |e| RouteDispatchError::BadRequest(format!("project_path: {e}")),
            )?;

        let thread_id = crate::services::thread_lifecycle::new_thread_id();

        let span = tracing::info_span!(
            "dispatch_launch_sse",
            route_id = self.route_id.as_str(),
            thread_id = thread_id.as_str(),
            item_ref_kind = item_ref.kind(),
        );

        let hub = state.event_streams.clone();
        let mut rx = hub.subscribe(&thread_id);

        let mut launch_handle = crate::routes::launch::spawn_dispatch_launch(
            state,
            item_ref,
            project_path,
            req.parameters,
            ctx.principal.id.clone(),
            ctx.principal.scopes.clone(),
            thread_id.clone(),
        );

        let events_svc = state.events.clone();
        let state_store_clone = state.state_store.clone();
        let keep_alive_secs = self.keep_alive_secs;
        let thread_id_for_stream = thread_id.clone();

        let stream = async_stream::stream! {
            let _guard = span.enter();
            yield Ok(
                axum::response::sse::Event::default()
                    .event("stream_started")
                    .data(serde_json::json!({"thread_id": thread_id_for_stream}).to_string())
            );

            // Track the highest chain_seq we've yielded so a lag
            // recovery can resume from there with `after_chain_seq`
            // (matching `thread_events`'s pattern). 0 is a safe
            // start — no event has chain_seq 0.
            let mut current_max: i64 = 0;
            let replay_batch_size = super::REPLAY_BATCH_SIZE;

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
                                // Paged replay matching thread_events:
                                // start at after_chain_seq = current_max,
                                // walk pages of `replay_batch_size`,
                                // dedup is implicit via current_max
                                // monotonicity.
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
                                // Persistence-first drain: when the
                                // launch task resolves successfully,
                                // any events the broadcast didn't
                                // deliver are still in the durable
                                // store. Replay from `after_chain_seq
                                // = current_max` until we hit the
                                // terminal lifecycle event (or the
                                // store runs out).
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
                                // Replay finished without a terminal
                                // event in the store. Check thread
                                // status: if terminal, that's fine —
                                // some lifecycle events are stored on
                                // a separate path. Otherwise emit a
                                // diagnostic.
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::raw::RawRequestBody;

    fn make_test_raw(id: &str, path: &str) -> RawRouteSpec {
        use crate::routes::raw::{RawLimits, RawRequest, RawRequestBody, RawResponseSpec};
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

    #[test]
    fn compile_rejects_auth_none() {
        let source = DispatchLaunchSource;
        let raw = make_test_raw("r1", "/execute/stream");
        let es = RawEventStreamResponse {
            source: "dispatch_launch".into(),
            source_config: serde_json::json!({"keep_alive_secs": 15}),
        };
        let ctx = SourceCompileContext {
            auth_verifier_key: "none",
        };
        let err = match source.compile(&raw, &es, &ctx) {
            Err(e) => e,
            Ok(_) => panic!("expected error"),
        };
        let msg = format!("{err}");
        assert!(msg.contains("requires auth 'rye_signed'"), "got: {msg}");
    }

    #[test]
    fn compile_rejects_non_json_body() {
        let source = DispatchLaunchSource;
        let mut raw = make_test_raw("r1", "/execute/stream");
        raw.request.body = RawRequestBody::Raw;
        let es = RawEventStreamResponse {
            source: "dispatch_launch".into(),
            source_config: serde_json::json!({"keep_alive_secs": 15}),
        };
        let ctx = SourceCompileContext {
            auth_verifier_key: "rye_signed",
        };
        let err = match source.compile(&raw, &es, &ctx) {
            Err(e) => e,
            Ok(_) => panic!("expected error"),
        };
        let msg = format!("{err}");
        assert!(msg.contains("requires request.body = json"), "got: {msg}");
    }

    #[test]
    fn compile_rejects_keep_alive_zero() {
        let source = DispatchLaunchSource;
        let raw = make_test_raw("r1", "/execute/stream");
        let es = RawEventStreamResponse {
            source: "dispatch_launch".into(),
            source_config: serde_json::json!({"keep_alive_secs": 0}),
        };
        let ctx = SourceCompileContext {
            auth_verifier_key: "rye_signed",
        };
        let err = match source.compile(&raw, &es, &ctx) {
            Err(e) => e,
            Ok(_) => panic!("expected error"),
        };
        let msg = format!("{err}");
        assert!(msg.contains("keep_alive_secs must be > 0"), "got: {msg}");
    }

    #[test]
    fn compile_rejects_unknown_source_config_key() {
        let source = DispatchLaunchSource;
        let raw = make_test_raw("r1", "/execute/stream");
        let es = RawEventStreamResponse {
            source: "dispatch_launch".into(),
            source_config: serde_json::json!({
                "keep_alive_secs": 15,
                "item_ref": "${request.body_json.item_ref}",
            }),
        };
        let ctx = SourceCompileContext {
            auth_verifier_key: "rye_signed",
        };
        let err = match source.compile(&raw, &es, &ctx) {
            Err(e) => e,
            Ok(_) => panic!("expected error"),
        };
        let msg = format!("{err}");
        // deny_unknown_fields on the typed struct produces this message.
        assert!(
            msg.contains("unknown field") && msg.contains("item_ref"),
            "got: {msg}"
        );
    }

    #[test]
    fn compile_rejects_missing_keep_alive_secs() {
        let source = DispatchLaunchSource;
        let raw = make_test_raw("r1", "/execute/stream");
        let es = RawEventStreamResponse {
            source: "dispatch_launch".into(),
            source_config: serde_json::json!({}),
        };
        let ctx = SourceCompileContext {
            auth_verifier_key: "rye_signed",
        };
        let err = match source.compile(&raw, &es, &ctx) {
            Err(e) => e,
            Ok(_) => panic!("expected error"),
        };
        let msg = format!("{err}");
        assert!(
            msg.contains("missing field `keep_alive_secs`"),
            "got: {msg}"
        );
    }

    #[test]
    fn compile_rejects_non_object_source_config() {
        let source = DispatchLaunchSource;
        let raw = make_test_raw("r1", "/execute/stream");
        let es = RawEventStreamResponse {
            source: "dispatch_launch".into(),
            source_config: serde_json::json!(123),
        };
        let ctx = SourceCompileContext {
            auth_verifier_key: "rye_signed",
        };
        let err = match source.compile(&raw, &es, &ctx) {
            Err(e) => e,
            Ok(_) => panic!("expected error"),
        };
        let msg = format!("{err}");
        assert!(msg.contains("invalid source_config"), "got: {msg}");
    }

    #[test]
    fn compile_succeeds_with_valid_config() {
        let source = DispatchLaunchSource;
        let raw = make_test_raw("r1", "/execute/stream");
        let es = RawEventStreamResponse {
            source: "dispatch_launch".into(),
            source_config: serde_json::json!({"keep_alive_secs": 10}),
        };
        let ctx = SourceCompileContext {
            auth_verifier_key: "rye_signed",
        };
        assert!(source.compile(&raw, &es, &ctx).is_ok());
    }

    #[test]
    fn execute_stream_request_rejects_missing_fields() {
        // No project_path, parameters
        let body = serde_json::json!({"item_ref": "directive:foo"});
        let bytes = serde_json::to_vec(&body).unwrap();
        let err = serde_json::from_slice::<LaunchRequest>(&bytes)
            .expect_err("must reject missing fields");
        let msg = err.to_string();
        assert!(
            msg.contains("missing field"),
            "expected missing-field error, got: {msg}"
        );
    }

    #[test]
    fn execute_stream_request_rejects_unknown_field() {
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
    fn execute_stream_request_accepts_complete_body() {
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
}
