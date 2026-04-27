pub mod abs_path;
pub mod compile;
pub mod dispatcher;
pub mod launch;
pub mod limits;
pub mod matcher;
pub mod parsed_ref;
pub mod raw;
pub mod reload;
pub mod response_modes;
pub mod streaming_sources;
pub mod verifiers;
pub mod webhook_dedupe;

use std::collections::HashMap;
use std::sync::Arc;

use axum::http::Method;

use compile::{
    CompiledRoute, ModeCompileContext,
};
use crate::dispatch_error::RouteConfigError;
use matcher::PathMatcher;
use raw::RawRouteSpec;
use response_modes::ResponseModeRegistry;
use streaming_sources::StreamingSourceRegistry;
use verifiers::AuthVerifierRegistry;

pub struct RouteTable {
    matcher: PathMatcher,
    pub all: Vec<Arc<CompiledRoute>>,
    pub fingerprint: String,
}

impl std::fmt::Debug for RouteTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RouteTable")
            .field("route_count", &self.all.len())
            .field("fingerprint", &self.fingerprint)
            .finish()
    }
}

impl RouteTable {
    pub fn match_request(
        &self,
        method: &Method,
        path: &str,
    ) -> Option<(Arc<CompiledRoute>, HashMap<String, String>)> {
        self.matcher.match_request(method, path)
    }

    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }
}

pub fn build_route_table(
    raw_routes: &[RawRouteSpec],
    verifier_registry: &AuthVerifierRegistry,
    mode_registry: &ResponseModeRegistry,
    streaming_sources: &StreamingSourceRegistry,
) -> Result<RouteTable, Vec<RouteConfigError>> {
    let mut errors: Vec<RouteConfigError> = Vec::new();
    let mut compiled: Vec<Arc<CompiledRoute>> = Vec::new();
    let mut seen_ids: HashMap<String, String> = HashMap::new();

    let ctx = ModeCompileContext {
        streaming_sources,
    };

    for raw in raw_routes {
        if let Some(first_source) = seen_ids.get(&raw.id) {
            errors.push(RouteConfigError::DuplicateRouteId {
                id: raw.id.clone(),
                first_source: first_source.clone(),
                second_source: raw.source_file.display().to_string(),
            });
            continue;
        }
        seen_ids.insert(raw.id.clone(), raw.source_file.display().to_string());

        let methods: Result<Vec<Method>, _> = raw
            .methods
            .iter()
            .map(|m| {
                m.parse::<Method>().map_err(|_| RouteConfigError::InvalidMethods {
                    id: raw.id.clone(),
                    reason: format!("unknown HTTP method '{m}'"),
                })
            })
            .collect();

        let methods = match methods {
            Ok(m) => m,
            Err(e) => {
                errors.push(e);
                continue;
            }
        };

        if methods.is_empty() {
            errors.push(RouteConfigError::InvalidMethods {
                id: raw.id.clone(),
                reason: "methods list is empty".to_string(),
            });
            continue;
        }

        // Built-in routes mounted in `build_router` (only `/health` +
        // `/execute` today). Config routes cannot shadow these. The
        // `/hook/` namespace is *config-owned* — there are no built-in
        // handlers under it — so config routes are free to register
        // there. Adding a built-in route here in the future also
        // requires adding it to the exact-match list below; routes
        // owned exclusively by config (e.g. `/execute/stream`,
        // `/threads/{id}/stream`, `/hook/<route-name>/...`) MUST NOT
        // appear in the reservation list.
        const RESERVED_EXACT: &[&str] = &["/health", "/execute"];

        let path = &raw.path;
        if let Some(r) = RESERVED_EXACT.iter().find(|r| path == *r) {
            errors.push(RouteConfigError::ReservedPathPrefix {
                id: raw.id.clone(),
                path: path.clone(),
                reserved: (*r).into(),
            });
            continue;
        }

        let verifier = match verifier_registry.get(&raw.auth) {
            Some(v) => v,
            None => {
                errors.push(RouteConfigError::UnknownVerifier {
                    id: raw.id.clone(),
                    name: raw.auth.clone(),
                });
                continue;
            }
        };

        let compiled_auth = match verifier.validate_route_config(&raw.id, raw.auth_config.as_ref()) {
            Ok(a) => a,
            Err(e) => {
                errors.push(e);
                continue;
            }
        };

        let mode = match mode_registry.get(&raw.response.mode) {
            Some(m) => m,
            None => {
                errors.push(RouteConfigError::UnknownResponseMode {
                    id: raw.id.clone(),
                    name: raw.response.mode.clone(),
                });
                continue;
            }
        };

        let compiled_mode = match mode.compile(raw, &ctx) {
            Ok(m) => m,
            Err(e) => {
                errors.push(e);
                continue;
            }
        };

        if raw.limits.timeout_ms == 0 && !mode.allows_zero_timeout() {
            errors.push(RouteConfigError::InvalidLimits {
                id: raw.id.clone(),
                reason: format!(
                    "timeout_ms = 0 is only valid for long-lived response modes; mode `{}` does not allow it",
                    raw.response.mode
                ),
            });
            continue;
        }

        compiled.push(Arc::new(CompiledRoute {
            id: raw.id.clone(),
            source_file: raw.source_file.clone(),
            path_pattern: raw.path.clone(),
            methods,
            auth: compiled_auth,
            limits: compile::CompiledLimits {
                body_bytes_max: raw.limits.body_bytes_max,
                timeout_ms: raw.limits.timeout_ms,
                concurrent_max: raw.limits.concurrent_max,
            },
            response_mode: compiled_mode,
            raw_response: raw.clone(),
            semaphore: std::sync::Arc::new(tokio::sync::Semaphore::new(raw.limits.concurrent_max as usize)),
        }));
    }

    if !errors.is_empty() {
        return Err(errors);
    }

    let matcher = PathMatcher::new(compiled.clone())
        .map_err(|e| vec![e])?;

    let fingerprint = {
        let mut ids: Vec<&str> = compiled.iter().map(|r| r.id.as_str()).collect();
        ids.sort();
        lillux::cas::sha256_hex(ids.join(",").as_bytes())
    };

    Ok(RouteTable {
        matcher,
        all: compiled,
        fingerprint,
    })
}

pub fn build_route_table_from_snapshot(
    snapshot: &crate::node_config::NodeConfigSnapshot,
) -> Result<RouteTable, Vec<RouteConfigError>> {
    let verifier_registry = AuthVerifierRegistry::with_builtins();
    let mode_registry = ResponseModeRegistry::with_builtins();
    let streaming_sources = StreamingSourceRegistry::with_builtins();
    build_route_table(&snapshot.routes, &verifier_registry, &mode_registry, &streaming_sources)
}

pub fn build_route_table_or_bail(
    snapshot: &crate::node_config::NodeConfigSnapshot,
) -> anyhow::Result<RouteTable> {
    build_route_table_from_snapshot(snapshot).map_err(|errors| {
        let msgs: Vec<String> = errors.iter().map(|e| e.to_string()).collect();
        anyhow::anyhow!(
            "route table build failed at startup ({} error(s)): {}",
            errors.len(),
            msgs.join("; ")
        )
    })
}

pub fn swap(state: &crate::state::AppState, new_table: Arc<RouteTable>) {
    state.route_table.store(new_table);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::raw::{
        RawLimits, RawRequestBody, RawRequest, RawResponseSpec, RawRouteSpec,
    };

    fn make_raw(id: &str, path: &str, methods: &[&str], auth: &str, mode: &str) -> RawRouteSpec {
        RawRouteSpec {
            section: "routes".to_string(),
            id: id.to_string(),
            path: path.to_string(),
            methods: methods.iter().map(|s| s.to_string()).collect(),
            auth: auth.to_string(),
            auth_config: None,
            limits: RawLimits::default(),
            response: RawResponseSpec {
                mode: mode.to_string(),
                source: None,
                source_config: serde_json::Value::Null,
                status: Some(200),
                content_type: Some("text/plain".to_string()),
                body_b64: Some("aGVsbG8=".to_string()),
            },
            execute: None,
            request: RawRequest {
                body: RawRequestBody::None,
            },
            source_file: std::path::PathBuf::from(format!("/test/{id}.yaml")),
        }
    }

    #[test]
    fn empty_routes_builds_empty_table() {
        let verifier_registry = AuthVerifierRegistry::with_builtins();
        let mode_registry = ResponseModeRegistry::with_builtins();
        let streaming_sources = StreamingSourceRegistry::with_builtins();
        let table = build_route_table(&[], &verifier_registry, &mode_registry, &streaming_sources).unwrap();
        assert!(table.all.is_empty());
        assert!(table.match_request(&Method::GET, "/anything").is_none());
    }

    #[test]
    fn single_valid_route() {
        let raw = make_raw("r1", "/api/test", &["GET"], "none", "static");
        let verifier_registry = AuthVerifierRegistry::with_builtins();
        let mode_registry = ResponseModeRegistry::with_builtins();
        let streaming_sources = StreamingSourceRegistry::with_builtins();
        let table = build_route_table(&[raw], &verifier_registry, &mode_registry, &streaming_sources).unwrap();
        assert_eq!(table.all.len(), 1);
        let (route, caps) = table.match_request(&Method::GET, "/api/test").unwrap();
        assert_eq!(route.id, "r1");
        assert!(caps.is_empty());
    }

    #[test]
    fn duplicate_id_rejected() {
        let r1 = make_raw("r1", "/a", &["GET"], "none", "static");
        let r2 = make_raw("r1", "/b", &["GET"], "none", "static");
        let verifier_registry = AuthVerifierRegistry::with_builtins();
        let mode_registry = ResponseModeRegistry::with_builtins();
        let streaming_sources = StreamingSourceRegistry::with_builtins();
        let err = build_route_table(&[r1, r2], &verifier_registry, &mode_registry, &streaming_sources).unwrap_err();
        assert_eq!(err.len(), 1);
        let msg = format!("{}", err[0]);
        assert!(msg.contains("duplicate route id"), "got: {msg}");
    }

    #[test]
    fn reserved_path_prefix_rejected() {
        let raw = make_raw("r1", "/health", &["GET"], "none", "static");
        let verifier_registry = AuthVerifierRegistry::with_builtins();
        let mode_registry = ResponseModeRegistry::with_builtins();
        let streaming_sources = StreamingSourceRegistry::with_builtins();
        let err = build_route_table(&[raw], &verifier_registry, &mode_registry, &streaming_sources).unwrap_err();
        let msg = format!("{}", err[0]);
        assert!(msg.contains("reserved path"), "got: {msg}");
    }

    #[test]
    fn reserved_exact_execute_rejected() {
        let raw = make_raw("r1", "/execute", &["POST"], "none", "static");
        let verifier_registry = AuthVerifierRegistry::with_builtins();
        let mode_registry = ResponseModeRegistry::with_builtins();
        let streaming_sources = StreamingSourceRegistry::with_builtins();
        let err = build_route_table(&[raw], &verifier_registry, &mode_registry, &streaming_sources).unwrap_err();
        let msg = format!("{}", err[0]);
        assert!(msg.contains("reserved path"), "got: {msg}");
    }

    #[test]
    fn hook_prefix_allowed_for_config_routes() {
        // The `/hook/` namespace is owned by config, not by built-in
        // routes. Webhook routes (Stripe, GitHub, Slack, ...) are
        // ordinary config routes that the loader admits without any
        // reservation check.
        let raw = make_raw("r1", "/hook/stripe", &["POST"], "none", "static");
        let verifier_registry = AuthVerifierRegistry::with_builtins();
        let mode_registry = ResponseModeRegistry::with_builtins();
        let streaming_sources = StreamingSourceRegistry::with_builtins();
        let table = build_route_table(
            &[raw],
            &verifier_registry,
            &mode_registry,
            &streaming_sources,
        )
        .expect("/hook/* must be admitted by the route table builder");
        assert_eq!(table.all.len(), 1);
    }

    #[test]
    fn execute_stream_subpath_allowed() {
        let raw = make_raw("r1", "/execute/stream", &["GET"], "none", "static");
        let verifier_registry = AuthVerifierRegistry::with_builtins();
        let mode_registry = ResponseModeRegistry::with_builtins();
        let streaming_sources = StreamingSourceRegistry::with_builtins();
        let table = build_route_table(&[raw], &verifier_registry, &mode_registry, &streaming_sources);
        assert!(table.is_ok(), "expected /execute/stream to be allowed");
    }

    #[test]
    fn status_not_reserved() {
        let raw = make_raw("r1", "/status", &["GET"], "none", "static");
        let verifier_registry = AuthVerifierRegistry::with_builtins();
        let mode_registry = ResponseModeRegistry::with_builtins();
        let streaming_sources = StreamingSourceRegistry::with_builtins();
        let table = build_route_table(&[raw], &verifier_registry, &mode_registry, &streaming_sources);
        assert!(table.is_ok(), "expected /status to be allowed through route-config");
    }

    #[test]
    fn empty_methods_rejected() {
        let mut raw = make_raw("r1", "/a", &["GET"], "none", "static");
        raw.methods.clear();
        let verifier_registry = AuthVerifierRegistry::with_builtins();
        let mode_registry = ResponseModeRegistry::with_builtins();
        let streaming_sources = StreamingSourceRegistry::with_builtins();
        let err = build_route_table(&[raw], &verifier_registry, &mode_registry, &streaming_sources).unwrap_err();
        let msg = format!("{}", err[0]);
        assert!(msg.contains("methods list is empty"), "got: {msg}");
    }

    #[test]
    fn http_allows_custom_methods() {
        let mut raw = make_raw("r1", "/a", &["GET"], "none", "static");
        raw.methods.insert("CUSTOM_METHOD".to_string());
        let verifier_registry = AuthVerifierRegistry::with_builtins();
        let mode_registry = ResponseModeRegistry::with_builtins();
        let streaming_sources = StreamingSourceRegistry::with_builtins();
        let table = build_route_table(&[raw], &verifier_registry, &mode_registry, &streaming_sources).unwrap();
        let (route, _) = table.match_request(&"CUSTOM_METHOD".parse().unwrap(), "/a").unwrap();
        assert_eq!(route.id, "r1");
    }

    #[test]
    fn fingerprint_is_deterministic() {
        let raw = make_raw("r1", "/a", &["GET"], "none", "static");
        let verifier_registry = AuthVerifierRegistry::with_builtins();
        let mode_registry = ResponseModeRegistry::with_builtins();
        let streaming_sources = StreamingSourceRegistry::with_builtins();
        let t1 = build_route_table(&[raw.clone()], &verifier_registry, &mode_registry, &streaming_sources).unwrap();
        let t2 = build_route_table(&[raw], &verifier_registry, &mode_registry, &streaming_sources).unwrap();
        assert_eq!(t1.fingerprint, t2.fingerprint);
    }

    #[test]
    fn multiple_errors_collected() {
        let r1 = make_raw("r1", "/health", &["GET"], "none", "static");
        let r2 = make_raw("r2", "/b", &["GET"], "nonexistent", "static");
        let verifier_registry = AuthVerifierRegistry::with_builtins();
        let mode_registry = ResponseModeRegistry::with_builtins();
        let streaming_sources = StreamingSourceRegistry::with_builtins();
        let err = build_route_table(&[r1, r2], &verifier_registry, &mode_registry, &streaming_sources).unwrap_err();
        assert!(err.len() >= 2, "expected >= 2 errors, got {}", err.len());
    }

    #[test]
    fn compiled_route_carries_semaphore_with_concurrent_max_permits() {
        let raw = make_raw("r1", "/api/test", &["GET"], "none", "static");
        let verifier_registry = AuthVerifierRegistry::with_builtins();
        let mode_registry = ResponseModeRegistry::with_builtins();
        let streaming_sources = StreamingSourceRegistry::with_builtins();
        let concurrent_max = raw.limits.concurrent_max;
        let table = build_route_table(&[raw], &verifier_registry, &mode_registry, &streaming_sources).unwrap();
        let route = &table.all[0];
        assert_eq!(route.semaphore.available_permits(), concurrent_max as usize);
    }

    #[test]
    fn build_route_table_or_bail_propagates_errors() {
        use crate::routes::raw::{RawLimits, RawRequest, RawRequestBody, RawResponseSpec};
        let bad_route = RawRouteSpec {
            section: "routes".to_string(),
            id: "dup".to_string(),
            path: "/health".to_string(),
            methods: ["GET".to_string()].into_iter().collect(),
            auth: "none".to_string(),
            auth_config: None,
            limits: RawLimits::default(),
            response: RawResponseSpec {
                mode: "static".to_string(),
                source: None,
                source_config: serde_json::Value::Null,
                status: Some(200),
                content_type: Some("text/plain".to_string()),
                body_b64: Some("aGVsbG8=".to_string()),
            },
            execute: None,
            request: RawRequest {
                body: RawRequestBody::None,
            },
            source_file: std::path::PathBuf::from("/test/dup.yaml"),
        };
        let snapshot = crate::node_config::NodeConfigSnapshot {
            bundles: vec![],
            routes: vec![bad_route],
        };
        let result = build_route_table_or_bail(&snapshot);
        assert!(result.is_err(), "expected error for reserved path /health");
    }

    #[test]
    fn build_route_table_or_bail_succeeds_on_empty() {
        let snapshot = crate::node_config::NodeConfigSnapshot {
            bundles: vec![],
            routes: vec![],
        };
        let table = build_route_table_or_bail(&snapshot).unwrap();
        assert!(table.all.is_empty());
    }

    #[test]
    fn zero_timeout_rejected_for_static_mode() {
        let mut raw = make_raw("r1", "/api/test", &["GET"], "none", "static");
        raw.limits.timeout_ms = 0;
        let verifier_registry = AuthVerifierRegistry::with_builtins();
        let mode_registry = ResponseModeRegistry::with_builtins();
        let streaming_sources = StreamingSourceRegistry::with_builtins();
        let err = build_route_table(&[raw], &verifier_registry, &mode_registry, &streaming_sources).unwrap_err();
        let msg = format!("{}", err[0]);
        assert!(msg.contains("timeout_ms = 0"), "got: {msg}");
    }

    #[test]
    fn zero_timeout_allowed_for_event_stream_mode() {
        let raw = RawRouteSpec {
            section: "routes".to_string(),
            id: "r1".to_string(),
            path: "/threads/{id}/stream".to_string(),
            methods: ["GET".to_string()].into_iter().collect(),
            auth: "rye_signed".to_string(),
            auth_config: Some(serde_json::json!({"public_key": "dummy"})),
            limits: RawLimits {
                timeout_ms: 0,
                ..RawLimits::default()
            },
            response: RawResponseSpec {
                mode: "event_stream".to_string(),
                source: Some("thread_events".to_string()),
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
            source_file: std::path::PathBuf::from("/test/r1.yaml"),
        };
        let verifier_registry = AuthVerifierRegistry::with_builtins();
        let mode_registry = ResponseModeRegistry::with_builtins();
        let streaming_sources = StreamingSourceRegistry::with_builtins();
        let result = build_route_table(&[raw], &verifier_registry, &mode_registry, &streaming_sources);
        assert!(result.is_ok(), "event_stream with timeout_ms=0 should be allowed");
    }
}
