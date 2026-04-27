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

pub struct DirectiveLaunchSource;

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

fn sse_error_event(message: &str) -> axum::response::sse::Event {
    axum::response::sse::Event::default()
        .event("stream_error")
        .data(serde_json::json!({"error": message}).to_string())
}

fn is_terminal_status(status: &str) -> bool {
    matches!(
        status,
        "completed" | "failed" | "cancelled" | "killed" | "timed_out"
    )
}

/// Typed body shape for `POST /execute/stream`.
///
/// Three required fields, no others. The verifier (`rye_signed`)
/// commits to the body bytes, so a typed deserialization with
/// `deny_unknown_fields` is the source-of-truth contract for what
/// the SSE source consumes — no template indirection, no
/// `Option<...>.unwrap_or(Value::Null)` silent fallbacks.
///
/// `parameters: Value` is required (must be present in the body), but
/// its inner shape is up to the directive's input schema.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExecuteStreamRequest {
    item_ref: String,
    project_path: String,
    parameters: Value,
}

impl StreamingSource for DirectiveLaunchSource {
    fn key(&self) -> &'static str {
        "directive_launch"
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
                src: "directive_launch".into(),
                required: REQUIRED_AUTH.into(),
                got: ctx.auth_verifier_key.into(),
            });
        }

        if raw_route.request.body != crate::routes::raw::RawRequestBody::Json {
            return Err(RouteConfigError::InvalidSourceConfig {
                id: raw_route.id.clone(),
                src: "directive_launch".into(),
                reason: "directive_launch requires request.body = json".into(),
            });
        }

        let cfg = &raw_event_stream.source_config;

        // Source_config is fixed: only `keep_alive_secs` is a knob.
        // The request body shape is hard-coded to `ExecuteStreamRequest`
        // (item_ref + project_path + parameters), so there is no
        // template indirection to validate.
        let keep_alive_secs = cfg
            .get("keep_alive_secs")
            .and_then(|v| v.as_u64())
            .unwrap_or(15);
        if keep_alive_secs == 0 {
            return Err(RouteConfigError::InvalidSourceConfig {
                id: raw_route.id.clone(),
                src: "directive_launch".into(),
                reason: "keep_alive_secs must be > 0".into(),
            });
        }

        // Reject any unknown keys in source_config so misconfigured
        // YAMLs surface at load time, not at first request.
        if let Some(obj) = cfg.as_object() {
            for k in obj.keys() {
                if k != "keep_alive_secs" {
                    return Err(RouteConfigError::InvalidSourceConfig {
                        id: raw_route.id.clone(),
                        src: "directive_launch".into(),
                        reason: format!(
                            "unknown source_config key '{k}'; allowed keys: [keep_alive_secs]"
                        ),
                    });
                }
            }
        }

        Ok(Arc::new(CompiledDirectiveLaunchSource {
            keep_alive_secs,
        }))
    }
}

struct CompiledDirectiveLaunchSource {
    keep_alive_secs: u64,
}

/// Spawn the dispatch task that turns a directive into a running
/// thread.  Calls `dispatch::dispatch` with `pre_minted_thread_id =
/// Some(thread_id)` so the resulting thread row uses the SSE-minted
/// id — the SSE subscriber registered against that id receives every
/// event from the very first lifecycle event onward.
fn build_launch_task(
    state: &AppState,
    item_ref: String,
    project_path: std::path::PathBuf,
    parameters: Value,
    principal_id: String,
    principal_scopes: Vec<String>,
    pre_minted_thread_id: String,
) -> tokio::task::JoinHandle<anyhow::Result<()>> {
    let state_clone = state.clone();

    tokio::spawn(async move {
        use ryeos_engine::canonical_ref::CanonicalRef;
        use ryeos_engine::contracts::{
            EffectivePrincipal, PlanContext, Principal, ProjectContext,
        };

        let site_id = state_clone.threads.site_id().to_string();

        let plan_ctx = PlanContext {
            requested_by: EffectivePrincipal::Local(Principal {
                fingerprint: principal_id.clone(),
                scopes: principal_scopes.clone(),
            }),
            project_context: ProjectContext::LocalPath {
                path: project_path.clone(),
            },
            current_site_id: site_id.clone(),
            origin_site_id: site_id,
            execution_hints: Default::default(),
            validate_only: false,
        };

        let exec_ctx = crate::service_executor::ExecutionContext {
            principal_fingerprint: principal_id.clone(),
            caller_scopes: principal_scopes,
            engine: state_clone.engine.clone(),
            plan_ctx,
        };

        let root_canonical = CanonicalRef::parse(&item_ref).map_err(|e| {
            anyhow::anyhow!("invalid item_ref '{item_ref}': {e}")
        })?;

        let dispatch_req = crate::dispatch::DispatchRequest {
            launch_mode: "inline",
            target_site_id: None,
            project_source_is_pushed_head: false,
            validate_only: false,
            params: parameters,
            acting_principal: principal_id.as_str(),
            project_path: project_path.as_path(),
            original_project_path: project_path.clone(),
            snapshot_hash: None,
            temp_dir: None,
            original_root_kind: root_canonical.kind.as_str(),
            pre_minted_thread_id: Some(pre_minted_thread_id.clone()),
        };

        match crate::dispatch::dispatch(&item_ref, &dispatch_req, &exec_ctx, &state_clone).await {
            Ok(crate::dispatch::DispatchOutcome::Unary(_)) => Ok(()),
            Ok(crate::dispatch::DispatchOutcome::Stream(_)) => {
                Err(anyhow::anyhow!(
                    "dispatch produced a streaming outcome — directive_launch \
                     cannot consume it (only Unary supported here)"
                ))
            }
            Err(e) => Err(anyhow::anyhow!(e.to_string())),
        }
    })
}

#[axum::async_trait]
impl BoundStreamingSource for CompiledDirectiveLaunchSource {
    async fn open(
        &self,
        ctx: &RouteDispatchContext,
        last_event_id: Option<i64>,
        state: &AppState,
    ) -> Result<SseEventStream, RouteDispatchError> {
        if last_event_id.is_some() {
            return Err(RouteDispatchError::BadRequest(
                "directive_launch does not support Last-Event-ID".into(),
            ));
        }

        // Typed body deserialization: the three required fields
        // (item_ref, project_path, parameters) MUST be present, with
        // no unknown extras. Missing fields → 400.
        let req: ExecuteStreamRequest = serde_json::from_slice(&ctx.body_raw).map_err(|e| {
            RouteDispatchError::BadRequest(format!("invalid request body: {e}"))
        })?;

        // `directive_launch` is the SSE source for *directive*
        // execution. Other kinds (tools, services, runtimes) have
        // their own dispatch paths; refusing them here keeps the
        // semantic contract tight.
        if !req.item_ref.starts_with("directive:") {
            return Err(RouteDispatchError::BadRequest(format!(
                "directive_launch only accepts 'directive:*' refs, got '{}'",
                req.item_ref
            )));
        }

        let project_path = std::path::PathBuf::from(&req.project_path);

        let thread_id = crate::services::thread_lifecycle::new_thread_id();

        let hub = state.event_streams.clone();
        let mut rx = hub.subscribe(&thread_id);

        let mut launch_handle = build_launch_task(
            state,
            req.item_ref,
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
            let replay_batch_size = 200usize;

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
                                    yield Ok(sse_error_event(&err_msg));
                                    return;
                                }

                                current_max = lag_max;

                                tracing::info!(
                                    thread_id = %thread_id_for_stream,
                                    lagged = n,
                                    "directive_launch SSE subscriber lag recovery complete"
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
                                            yield Ok(sse_error_event(&format!("post-launch replay failed: {e}")));
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
                                yield Ok(sse_error_event("launch completed but thread is not terminal"));
                                return;
                            }
                            Ok(Err(e)) => {
                                yield Ok(sse_error_event(&format!("launch failed: {e}")));
                                return;
                            }
                            Err(_) => {
                                yield Ok(sse_error_event("launch task panicked"));
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
                source: Some("directive_launch".into()),
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
        let source = DirectiveLaunchSource;
        let raw = make_test_raw("r1", "/execute/stream");
        let es = RawEventStreamResponse {
            source: "directive_launch".into(),
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
        let source = DirectiveLaunchSource;
        let mut raw = make_test_raw("r1", "/execute/stream");
        raw.request.body = RawRequestBody::Raw;
        let es = RawEventStreamResponse {
            source: "directive_launch".into(),
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
        let source = DirectiveLaunchSource;
        let raw = make_test_raw("r1", "/execute/stream");
        let es = RawEventStreamResponse {
            source: "directive_launch".into(),
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
        let source = DirectiveLaunchSource;
        let raw = make_test_raw("r1", "/execute/stream");
        let es = RawEventStreamResponse {
            source: "directive_launch".into(),
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
        assert!(
            msg.contains("unknown source_config key 'item_ref'"),
            "got: {msg}"
        );
    }

    #[test]
    fn compile_succeeds_with_valid_config() {
        let source = DirectiveLaunchSource;
        let raw = make_test_raw("r1", "/execute/stream");
        let es = RawEventStreamResponse {
            source: "directive_launch".into(),
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
        let err = serde_json::from_slice::<ExecuteStreamRequest>(&bytes)
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
        let err = serde_json::from_slice::<ExecuteStreamRequest>(&bytes)
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
        let req: ExecuteStreamRequest =
            serde_json::from_slice(&bytes).expect("valid body must parse");
        assert_eq!(req.item_ref, "directive:my/agent");
        assert_eq!(req.project_path, "/tmp/proj");
        assert_eq!(req.parameters["name"], "World");
    }
}
