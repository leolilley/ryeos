//! `thread_detail` read source — fetch one thread row plus result, artifacts,
//! and facets. Mirrors the JSON shape of `services::handlers::threads_get`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde_json::Value;

use crate::dispatch_error::{RouteConfigError, RouteDispatchError};
use crate::routes::raw::RawRouteSpec;
use crate::routes::read_sources::{BoundReadSource, ReadSource};
use crate::state::AppState;

pub struct ThreadDetailSource;

impl ReadSource for ThreadDetailSource {
    fn key(&self) -> &'static str {
        "thread_detail"
    }

    fn compile(
        &self,
        raw_route: &RawRouteSpec,
        source_config: &Value,
    ) -> Result<Arc<dyn BoundReadSource>, RouteConfigError> {
        let thread_id_template = source_config
            .get("thread_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| RouteConfigError::InvalidSourceConfig {
                id: raw_route.id.clone(),
                src: "thread_detail".into(),
                reason: "missing 'thread_id' in source_config".into(),
            })?;

        validate_path_only_interpolation(thread_id_template, &raw_route.id)?;

        let capture_name = extract_path_capture_name(thread_id_template, &raw_route.id)?;

        let declared_captures = extract_path_captures(&raw_route.path);
        if !declared_captures.contains(&capture_name) {
            return Err(RouteConfigError::InvalidSourceConfig {
                id: raw_route.id.clone(),
                src: "thread_detail".into(),
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

        Ok(Arc::new(CompiledThreadDetailSource {
            thread_id_capture: capture_name,
        }))
    }
}

struct CompiledThreadDetailSource {
    thread_id_capture: String,
}

#[axum::async_trait]
impl BoundReadSource for CompiledThreadDetailSource {
    async fn fetch(
        &self,
        captures: &HashMap<String, String>,
        state: &AppState,
    ) -> Result<Option<Value>, RouteDispatchError> {
        let thread_id = captures
            .get(&self.thread_id_capture)
            .ok_or_else(|| {
                RouteDispatchError::Internal(format!(
                    "thread_detail: path capture '{}' not found in route match",
                    self.thread_id_capture
                ))
            })?;

        match state.threads.get_thread(thread_id) {
            Ok(Some(_)) => {
                let facets = state.state_store.get_facets(thread_id).map_err(|e| {
                    RouteDispatchError::Internal(format!("get_facets failed: {e}"))
                })?;
                let facets_map: HashMap<&str, &str> =
                    facets.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();

                let detail = state.threads.get_thread(thread_id).map_err(|e| {
                    RouteDispatchError::Internal(format!("get_thread failed: {e}"))
                })?;

                let result = state.threads.get_thread_result(thread_id).map_err(|e| {
                    RouteDispatchError::Internal(format!("get_thread_result failed: {e}"))
                })?;

                let artifacts = state.threads.list_thread_artifacts(thread_id).map_err(|e| {
                    RouteDispatchError::Internal(format!("list_thread_artifacts failed: {e}"))
                })?;

                Ok(Some(serde_json::json!({
                    "thread": detail,
                    "result": result,
                    "artifacts": artifacts,
                    "facets": facets_map,
                })))
            }
            Ok(None) => Ok(None),
            Err(e) => Err(RouteDispatchError::Internal(format!(
                "get_thread failed: {e}"
            ))),
        }
    }
}

// ---------------------------------------------------------------------------
// Path capture helpers (same logic as streaming_sources/thread_events.rs)
// ---------------------------------------------------------------------------

fn validate_path_only_interpolation(
    template: &str,
    route_id: &str,
) -> Result<(), RouteConfigError> {
    if let Some(start) = template.find("${") {
        if let Some(end) = template[start..].find('}') {
            let inner = &template[start + 2..start + end];
            if !inner.starts_with("path.") {
                return Err(RouteConfigError::InvalidSourceConfig {
                    id: route_id.into(),
                    src: "thread_detail".into(),
                    reason: format!(
                        "thread_id must use ${{path.<name>}} interpolation, got ${{{inner}}}"
                    ),
                });
            }
        } else {
            return Err(RouteConfigError::InvalidSourceConfig {
                id: route_id.into(),
                src: "thread_detail".into(),
                reason: "thread_id contains unterminated '${' template".into(),
            });
        }

        let after_first = &template[start + 2..];
        if after_first.find("${").is_some() {
            return Err(RouteConfigError::InvalidSourceConfig {
                id: route_id.into(),
                src: "thread_detail".into(),
                reason: "thread_id must be a single ${path.<name>} template".into(),
            });
        }
    } else {
        return Err(RouteConfigError::InvalidSourceConfig {
            id: route_id.into(),
            src: "thread_detail".into(),
            reason: "thread_id must use ${path.<name>}} interpolation".into(),
        });
    }
    Ok(())
}

fn extract_path_capture_name(
    template: &str,
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
        src: "thread_detail".into(),
        reason: "thread_id has invalid path capture template".into(),
    })
}

fn extract_path_captures(path: &str) -> HashSet<String> {
    let mut captures = HashSet::new();
    for segment in path.split('/').skip(1) {
        if let Some(name) = segment
            .strip_prefix('{')
            .and_then(|s| s.strip_suffix('}'))
        {
            captures.insert(name.to_string());
        }
    }
    captures
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::read_sources::ReadSourceRegistry;
    use crate::routes::raw::{
        RawLimits, RawRequest, RawRequestBody, RawResponseSpec, RawRouteSpec,
    };

    fn make_raw(path: &str, source_config: Value) -> RawRouteSpec {
        RawRouteSpec {
            section: "routes".into(),
            id: "test-route".into(),
            path: path.into(),
            methods: ["GET".into()].into_iter().collect(),
            auth: "rye_signed".into(),
            auth_config: None,
            limits: RawLimits::default(),
            response: RawResponseSpec {
                mode: "read".into(),
                source: Some("thread_detail".into()),
                source_config,
                status: None,
                content_type: None,
                body_b64: None,
            },
            execute: None,
            request: RawRequest {
                body: RawRequestBody::None,
            },
            source_file: std::path::PathBuf::from("/test/r.yaml"),
        }
    }

    #[test]
    fn compile_valid_config() {
        let src = ThreadDetailSource;
        let raw = make_raw(
            "/threads/{thread_id}",
            serde_json::json!({ "thread_id": "${path.thread_id}" }),
        );
        let result = src.compile(&raw, &raw.response.source_config);
        assert!(result.is_ok(), "expected Ok, got: {:?}", result.err());
    }

    #[test]
    fn compile_rejects_missing_thread_id() {
        let src = ThreadDetailSource;
        let raw = make_raw("/threads/{thread_id}", serde_json::json!({}));
        let result = src.compile(&raw, &raw.response.source_config);
        let err = match result {
            Err(e) => e,
            Ok(_) => panic!("expected error"),
        };
        let msg = format!("{err}");
        assert!(msg.contains("missing 'thread_id'"), "got: {msg}");
    }

    #[test]
    fn compile_rejects_non_path_interpolation() {
        let src = ThreadDetailSource;
        let raw = make_raw(
            "/threads/{thread_id}",
            serde_json::json!({ "thread_id": "fixed-value" }),
        );
        let result = src.compile(&raw, &raw.response.source_config);
        let err = match result {
            Err(e) => e,
            Ok(_) => panic!("expected error"),
        };
        let msg = format!("{err}");
        assert!(
            msg.contains("must use ${path.<name>}"),
            "got: {msg}"
        );
    }

    #[test]
    fn compile_rejects_undeclared_capture() {
        let src = ThreadDetailSource;
        let raw = make_raw(
            "/threads/{id}",
            serde_json::json!({ "thread_id": "${path.thread_id}" }),
        );
        let result = src.compile(&raw, &raw.response.source_config);
        let err = match result {
            Err(e) => e,
            Ok(_) => panic!("expected error"),
        };
        let msg = format!("{err}");
        assert!(
            msg.contains("undeclared path capture 'thread_id'"),
            "got: {msg}"
        );
    }

    #[test]
    fn registry_finds_thread_detail() {
        let reg = ReadSourceRegistry::with_builtins();
        assert!(reg.get("thread_detail").is_some());
    }
}
