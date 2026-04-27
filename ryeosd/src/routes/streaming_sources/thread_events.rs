use std::collections::HashSet;
use std::sync::Arc;

use crate::dispatch_error::{RouteConfigError, RouteDispatchError};
use crate::routes::compile::RouteDispatchContext;
use crate::routes::raw::RawRouteSpec;
use crate::routes::streaming_sources::{
    BoundStreamingSource, RawEventStreamResponse, SseEventStream, SourceCompileContext,
    StreamingSource,
};
use crate::services::event_store::EventReplayParams;
use crate::state::AppState;
use crate::state_store::PersistedEventRecord;

pub struct ThreadEventsSource;

const TERMINAL_EVENT_TYPES: &[&str] = &[
    "thread_completed",
    "thread_failed",
    "thread_cancelled",
    "thread_killed",
    "thread_timed_out",
];

const REQUIRED_AUTH: &str = "rye_signed";

impl StreamingSource for ThreadEventsSource {
    fn key(&self) -> &'static str {
        "thread_events"
    }

    fn compile(
        &self,
        raw_route: &RawRouteSpec,
        _raw_event_stream: &RawEventStreamResponse,
        ctx: &SourceCompileContext,
    ) -> Result<Arc<dyn BoundStreamingSource>, RouteConfigError> {
        if ctx.auth_verifier_key != REQUIRED_AUTH {
            return Err(RouteConfigError::SourceAuthRequirement {
                id: raw_route.id.clone(),
                src: "thread_events".into(),
                required: REQUIRED_AUTH.into(),
                got: ctx.auth_verifier_key.into(),
            });
        }

        let source_config = &_raw_event_stream.source_config;

        let thread_id_template = source_config
            .get("thread_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| RouteConfigError::InvalidSourceConfig {
                id: raw_route.id.clone(),
                src: "thread_events".into(),
                reason: "missing 'thread_id' in source_config".into(),
            })?;

        validate_path_only_interpolation(thread_id_template, "thread_id", &raw_route.id)?;

        let capture_name = extract_path_capture_name(thread_id_template, "thread_id", &raw_route.id)?;

        let declared_captures = extract_path_captures(&raw_route.path);
        if !declared_captures.contains(&capture_name) {
            return Err(RouteConfigError::InvalidSourceConfig {
                id: raw_route.id.clone(),
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
                id: raw_route.id.clone(),
                src: "thread_events".into(),
                reason: "keep_alive_secs must be > 0".into(),
            });
        }

        Ok(Arc::new(CompiledThreadEventsSource {
            thread_id_capture: capture_name,
            keep_alive_secs,
        }))
    }
}

fn validate_path_only_interpolation(
    template: &str,
    field: &str,
    route_id: &str,
) -> Result<(), RouteConfigError> {
    if let Some(start) = template.find("${") {
        if let Some(end) = template[start..].find('}') {
            let inner = &template[start + 2..start + end];
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
                reason: format!("{field} contains unterminated '${{' template"),
            });
        }

        let after_first = &template[start + 2..];
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
    Ok(())
}

fn extract_path_capture_name(
    template: &str,
    field: &str,
    route_id: &str,
) -> Result<String, RouteConfigError> {
    let trimmed = template.trim();
    let prefix = "${path.";
    let suffix = "}";
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

fn extract_path_captures(path: &str) -> HashSet<String> {
    let mut captures = HashSet::new();
    for segment in path.split('/').skip(1) {
        if let Some(name) = segment.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
            captures.insert(name.to_string());
        }
    }
    captures
}

struct CompiledThreadEventsSource {
    thread_id_capture: String,
    keep_alive_secs: u64,
}

fn sse_event_for_persisted(ev: &PersistedEventRecord) -> axum::response::sse::Event {
    axum::response::sse::Event::default()
        .event(ev.event_type.clone())
        .id(ev.chain_seq.to_string())
        .data(serde_json::to_string(ev).expect("PersistedEventRecord serializes"))
}

fn is_terminal(event_type: &str) -> bool {
    TERMINAL_EVENT_TYPES.contains(&event_type)
}

fn sse_error_event(message: &str) -> axum::response::sse::Event {
    axum::response::sse::Event::default()
        .event("stream_error")
        .data(serde_json::json!({"error": message}).to_string())
}

#[axum::async_trait]
impl BoundStreamingSource for CompiledThreadEventsSource {
    async fn open(
        &self,
        ctx: &RouteDispatchContext,
        last_event_id: Option<i64>,
        state: &AppState,
    ) -> Result<SseEventStream, RouteDispatchError> {
        let thread_id = ctx
            .captures
            .get(&self.thread_id_capture)
            .ok_or_else(|| {
                RouteDispatchError::Internal(format!(
                    "path capture '{}' not found in request",
                    self.thread_id_capture
                ))
            })?
            .clone();

        let thread_detail = state
            .state_store
            .get_thread(&thread_id)
            .map_err(|e| RouteDispatchError::Internal(e.to_string()))?
            .ok_or(RouteDispatchError::NotFound)?;

        let principal_id = &ctx.principal.id;
        let requested_by = match &thread_detail.requested_by {
            Some(r) => r,
            None => return Err(RouteDispatchError::NotFound),
        };

        if principal_id != requested_by {
            return Err(RouteDispatchError::NotFound);
        }

        let hub = state.event_streams.clone();
        let mut rx = hub.subscribe(&thread_id);

        let last_seen = last_event_id;

        let events_svc = state.events.clone();
        let state_store_clone = state.state_store.clone();

        let keep_alive_secs = self.keep_alive_secs;

        let stream = async_stream::stream! {
            yield Ok(
                axum::response::sse::Event::default()
                    .event("stream_started")
                    .data(serde_json::json!({"thread_id": thread_id}).to_string())
            );

            let replay_batch_size = 200usize;

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
                                yield Ok(sse_error_event(&format!("replay failed: {e}")));
                                break;
                            }
                        }
                    }

                    let mut buffered: Vec<PersistedEventRecord> = Vec::new();
                    while let Ok(ev) = rx.try_recv() {
                        if ev.chain_seq > max_seq {
                            buffered.push(ev);
                        }
                    }
                    buffered.sort_by_key(|e| e.chain_seq);

                    for ev in &buffered {
                        if ev.chain_seq > max_seq {
                            max_seq = ev.chain_seq;
                            yield Ok(sse_event_for_persisted(ev));
                            yielded_any_replay_event = true;
                            if is_terminal(&ev.event_type) {
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
                                    yield Ok(sse_error_event(&err_msg));
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
                    yield Ok(sse_error_event(&format!("replay failed: {e}")));
                    return;
                }
            }
        };

        Ok(SseEventStream {
            stream: Box::pin(stream),
            keep_alive_secs,
        })
    }
}

fn is_terminal_status(status: &str) -> bool {
    matches!(
        status,
        "completed" | "failed" | "cancelled" | "killed" | "timed_out"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_path_capture_valid() {
        assert_eq!(
            extract_path_capture_name("${path.thread_id}", "thread_id", "r1").unwrap(),
            "thread_id"
        );
    }

    #[test]
    fn extract_path_capture_rejects_non_path() {
        assert!(extract_path_capture_name("${query.id}", "thread_id", "r1").is_err());
    }

    #[test]
    fn validate_path_only_accepts_path_prefix() {
        assert!(validate_path_only_interpolation("${path.x}", "f", "r1").is_ok());
        assert!(
            validate_path_only_interpolation("${query.x}", "f", "r1").is_err()
        );
        assert!(validate_path_only_interpolation("static", "f", "r1").is_err());
    }

    #[test]
    fn extract_path_captures_finds_all() {
        let caps = extract_path_captures("/threads/{id}/events/{sub}");
        assert!(caps.contains("id"));
        assert!(caps.contains("sub"));
        assert_eq!(caps.len(), 2);
    }

    #[test]
    fn validate_path_only_rejects_double_interpolation() {
        assert!(
            validate_path_only_interpolation("${path.x}-${path.y}", "f", "r1").is_err()
        );
    }

    #[test]
    fn compile_rejects_auth_none() {
        let source = ThreadEventsSource;
        let raw = make_test_raw("r1", "/threads/{id}/stream");
        let es = RawEventStreamResponse {
            source: "thread_events".into(),
            source_config: serde_json::json!({
                "thread_id": "${path.id}",
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
        assert!(
            msg.contains("requires auth 'rye_signed'"),
            "got: {msg}"
        );
    }

    #[test]
    fn compile_rejects_non_path_interpolation() {
        let source = ThreadEventsSource;
        let raw = make_test_raw("r1", "/threads/{id}/stream");
        let es = RawEventStreamResponse {
            source: "thread_events".into(),
            source_config: serde_json::json!({
                "thread_id": "${query.id}",
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
        assert!(msg.contains("must use ${path."), "got: {msg}");
    }

    #[test]
    fn compile_rejects_undeclared_capture() {
        let source = ThreadEventsSource;
        let raw = make_test_raw("r1", "/threads/{tid}/stream");
        let es = RawEventStreamResponse {
            source: "thread_events".into(),
            source_config: serde_json::json!({
                "thread_id": "${path.wrong}",
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
        assert!(msg.contains("undeclared path capture"), "got: {msg}");
    }

    #[test]
    fn compile_rejects_keep_alive_zero() {
        let source = ThreadEventsSource;
        let raw = make_test_raw("r1", "/threads/{id}/stream");
        let es = RawEventStreamResponse {
            source: "thread_events".into(),
            source_config: serde_json::json!({
                "thread_id": "${path.id}",
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
        let source = ThreadEventsSource;
        let raw = make_test_raw("r1", "/threads/{id}/stream");
        let es = RawEventStreamResponse {
            source: "thread_events".into(),
            source_config: serde_json::json!({
                "thread_id": "${path.id}",
                "keep_alive_secs": 10,
            }),
        };
        let ctx = SourceCompileContext {
            auth_verifier_key: "rye_signed",
        };
        assert!(source.compile(&raw, &es, &ctx).is_ok());
    }

    #[test]
    fn is_terminal_status_detects_completed() {
        assert!(is_terminal_status("completed"));
        assert!(is_terminal_status("failed"));
        assert!(is_terminal_status("cancelled"));
        assert!(is_terminal_status("killed"));
        assert!(is_terminal_status("timed_out"));
        assert!(!is_terminal_status("running"));
        assert!(!is_terminal_status("pending"));
    }

    fn make_test_raw(id: &str, path: &str) -> RawRouteSpec {
        use crate::routes::raw::{RawLimits, RawRequest, RawRequestBody, RawResponseSpec};
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
}
