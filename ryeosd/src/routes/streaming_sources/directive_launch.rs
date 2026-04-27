use std::collections::HashMap;
use std::sync::Arc;

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

        let item_ref = cfg
            .get("item_ref")
            .and_then(|v| v.as_str())
            .ok_or_else(|| RouteConfigError::InvalidSourceConfig {
                id: raw_route.id.clone(),
                src: "directive_launch".into(),
                reason: "missing 'item_ref' in source_config".into(),
            })?;

        if !item_ref.starts_with("${request.body_json.") || !item_ref.ends_with('}') {
            return Err(RouteConfigError::InvalidSourceConfig {
                id: raw_route.id.clone(),
                src: "directive_launch".into(),
                reason: "item_ref must use ${request.body_json.<key>} interpolation".into(),
            });
        }
        let item_ref_path = item_ref
            .trim_start_matches("${request.body_json.")
            .trim_end_matches('}')
            .to_string();

        let parameters = cfg.get("parameters");
        let parameters_path = match parameters {
            Some(v) => {
                let s = v.as_str().ok_or_else(|| RouteConfigError::InvalidSourceConfig {
                    id: raw_route.id.clone(),
                    src: "directive_launch".into(),
                    reason: "parameters must be a string template".into(),
                })?;
                if !s.starts_with("${request.body_json.") || !s.ends_with('}') {
                    return Err(RouteConfigError::InvalidSourceConfig {
                        id: raw_route.id.clone(),
                        src: "directive_launch".into(),
                        reason: "parameters must use ${request.body_json.<key>} interpolation".into(),
                    });
                }
                Some(
                    s.trim_start_matches("${request.body_json.")
                        .trim_end_matches('}')
                        .to_string(),
                )
            }
            None => None,
        };

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

        Ok(Arc::new(CompiledDirectiveLaunchSource {
            item_ref_path,
            parameters_path,
            keep_alive_secs,
        }))
    }
}

struct CompiledDirectiveLaunchSource {
    item_ref_path: String,
    parameters_path: Option<String>,
    keep_alive_secs: u64,
}

fn extract_dotted_value<'a>(body: &'a Value, dotted_path: &str) -> Option<&'a Value> {
    let mut current = body;
    for key in dotted_path.split('.') {
        current = current.get(key)?;
    }
    Some(current)
}

// FIXME (§E.3 residual gap): this helper currently goes via
// `services::thread_lifecycle::resolve_root_execution` + `execution::launch::build_and_launch`
// directly, which requires `metadata.executor_id` to be set on the
// resolved item. The `directive` kind schema has no extraction rule
// for `executor_id`; production directives alias through
// `runtime:directive-runtime` via `dispatch::dispatch`'s kind-alias
// loop. As a consequence, `/execute/stream` cannot launch standard
// directives today — it fails with `"item ... does not declare an
// executor_id"`. The fix is to plumb `pre_minted_thread_id` into
// `dispatch::DispatchRequest` (then through to
// `services::thread_lifecycle::create_root_thread_with_id`, which
// already accepts a pre-minted id per E.1) and call `dispatch::dispatch`
// here instead. The Phase E e2e round-trip test
// (`sse_directive_launch_e2e_round_trip`) is gated by `#[ignore]`
// until this lands. The other two Phase E tests cover wiring +
// thread-id mint contract.
fn build_launch_task(
    state: &AppState,
    item_ref: &str,
    parameters: Value,
    principal_id: &str,
    principal_scopes: &[String],
    project_path: Option<&std::path::Path>,
    pre_minted_thread_id: String,
) -> tokio::task::JoinHandle<anyhow::Result<()>> {
    let state_clone = state.clone();
    let item_ref_owned = item_ref.to_string();
    let principal_id_owned = principal_id.to_string();
    let principal_scopes_owned: Vec<String> = principal_scopes.to_vec();
    let project_path_owned = project_path.map(|p| p.to_path_buf());

    tokio::spawn(async move {
        let project_path = match &project_path_owned {
            Some(p) => p,
            None => {
                anyhow::bail!("directive_launch requires a project_path");
            }
        };

        let site_id = state_clone.threads.site_id();

        let resolved = crate::services::thread_lifecycle::resolve_root_execution(
            &state_clone.engine,
            site_id,
            project_path,
            &item_ref_owned,
            "inline",
            parameters,
            Some(principal_id_owned.clone()),
            principal_scopes_owned.clone(),
            false,
        )?;

        let executor_ref = resolved
            .resolved_item
            .metadata
            .executor_id
            .clone()
            .ok_or_else(|| {
                anyhow::anyhow!("item {} does not declare an executor_id", item_ref_owned)
            })?;

        crate::execution::launch::build_and_launch(
            &state_clone,
            &executor_ref,
            &principal_id_owned,
            &resolved,
            project_path,
            &resolved.parameters,
            &HashMap::new(),
            Some(&pre_minted_thread_id),
        )
        .await?;

        Ok(())
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

        let body: Value = serde_json::from_slice(&ctx.body_raw).map_err(|e| {
            RouteDispatchError::BadRequest(format!("invalid JSON body: {e}"))
        })?;

        let item_ref = extract_dotted_value(&body, &self.item_ref_path)
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                RouteDispatchError::BadRequest(format!(
                    "body missing field '{}'",
                    self.item_ref_path
                ))
            })?
            .to_string();

        let parameters = match &self.parameters_path {
            Some(path) => extract_dotted_value(&body, path)
                .cloned()
                .unwrap_or(Value::Null),
            None => Value::Null,
        };

        let thread_id = crate::services::thread_lifecycle::new_thread_id();

        let hub = state.event_streams.clone();
        let mut rx = hub.subscribe(&thread_id);

        let mut launch_handle = build_launch_task(
            state,
            &item_ref,
            parameters,
            &ctx.principal.id,
            &ctx.principal.scopes,
            ctx.project_path.as_deref(),
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

            loop {
                tokio::select! {
                    recv_result = rx.recv() => {
                        match recv_result {
                            Ok(ev) => {
                                let event_type = ev.event_type.clone();
                                yield Ok(
                                    axum::response::sse::Event::default()
                                        .event(ev.event_type.clone())
                                        .id(ev.chain_seq.to_string())
                                        .data(serde_json::to_string(&ev).expect("PersistedEventRecord serializes"))
                                );
                                if is_terminal(&event_type) {
                                    return;
                                }
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                                let replay_result = events_svc
                                    .replay(&EventReplayParams {
                                        chain_root_id: None,
                                        thread_id: Some(thread_id_for_stream.clone()),
                                        after_chain_seq: None,
                                        limit: 200,
                                    });
                                match replay_result {
                                    Ok(result) => {
                                        for ev in &result.events {
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
                                    Err(e) => {
                                        yield Ok(sse_error_event(&format!("lag replay failed: {e}")));
                                        return;
                                    }
                                }
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
                    "item_ref": "${request.body_json.item_ref}",
                    "parameters": "${request.body_json.parameters}",
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
            source_config: serde_json::json!({
                "item_ref": "${request.body_json.item_ref}",
                "parameters": "${request.body_json.parameters}",
                "keep_alive_secs": 15,
            }),
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
            source_config: serde_json::json!({
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
        assert!(msg.contains("requires request.body = json"), "got: {msg}");
    }

    #[test]
    fn compile_rejects_missing_item_ref() {
        let source = DirectiveLaunchSource;
        let raw = make_test_raw("r1", "/execute/stream");
        let es = RawEventStreamResponse {
            source: "directive_launch".into(),
            source_config: serde_json::json!({
                "keep_alive_secs": 15,
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
        assert!(msg.contains("missing 'item_ref'"), "got: {msg}");
    }

    #[test]
    fn compile_rejects_keep_alive_zero() {
        let source = DirectiveLaunchSource;
        let raw = make_test_raw("r1", "/execute/stream");
        let es = RawEventStreamResponse {
            source: "directive_launch".into(),
            source_config: serde_json::json!({
                "item_ref": "${request.body_json.item_ref}",
                "keep_alive_secs": 0,
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
        assert!(msg.contains("keep_alive_secs must be > 0"), "got: {msg}");
    }

    #[test]
    fn compile_succeeds_with_valid_config() {
        let source = DirectiveLaunchSource;
        let raw = make_test_raw("r1", "/execute/stream");
        let es = RawEventStreamResponse {
            source: "directive_launch".into(),
            source_config: serde_json::json!({
                "item_ref": "${request.body_json.item_ref}",
                "parameters": "${request.body_json.parameters}",
                "keep_alive_secs": 10,
            }),
        };
        let ctx = SourceCompileContext {
            auth_verifier_key: "rye_signed",
        };
        assert!(source.compile(&raw, &es, &ctx).is_ok());
    }

    #[test]
    fn extract_dotted_value_nested() {
        let body = serde_json::json!({"a": {"b": {"c": 42}}});
        assert_eq!(
            extract_dotted_value(&body, "a.b.c"),
            Some(&serde_json::json!(42))
        );
    }

    #[test]
    fn extract_dotted_value_missing() {
        let body = serde_json::json!({"a": 1});
        assert_eq!(extract_dotted_value(&body, "x.y.z"), None);
    }
}
