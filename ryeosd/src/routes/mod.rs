pub mod abs_path;
pub mod compile;
pub mod dispatcher;
pub mod interpolation;
pub mod invocation;
pub mod invokers;
pub mod launch;
pub mod limits;
pub mod matcher;
pub mod parsed_ref;
pub mod raw;
pub mod reload;
pub mod response_modes;
pub mod webhook_dedupe;use std::collections::HashMap;
use std::sync::Arc;

use axum::http::Method;

use compile::CompiledRoute;
use crate::dispatch_error::RouteConfigError;
use matcher::PathMatcher;
use raw::RawRouteSpec;
use response_modes::ResponseModeRegistry;

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
    mode_registry: &ResponseModeRegistry,
) -> Result<RouteTable, Vec<RouteConfigError>> {
    let mut errors: Vec<RouteConfigError> = Vec::new();
    let mut compiled: Vec<Arc<CompiledRoute>> = Vec::new();
    let mut seen_ids: HashMap<String, String> = HashMap::new();

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

        // Compile auth invoker (no registry lookup).
        let auth_invoker = match invokers::compile_auth_invoker(
            &raw.auth,
            raw.auth_config.as_ref(),
            &raw.id,
        ) {
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

        let compiled_mode = match mode.compile(raw) {
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
            auth_invoker,
            limits: compile::CompiledLimits {
                body_bytes_max: raw.limits.body_bytes_max,
                timeout_ms: raw.limits.timeout_ms,
                concurrent_max: raw.limits.concurrent_max,
            },
            response_mode: compiled_mode,
            raw_response: raw.clone(),
            semaphore: Arc::new(tokio::sync::Semaphore::new(
                raw.limits.concurrent_max as usize,
            )),
        }));
    }

    if !errors.is_empty() {
        return Err(errors);
    }

    let matcher = PathMatcher::new(compiled.clone()).map_err(|e| vec![e])?;

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
    let mode_registry = ResponseModeRegistry::with_builtins();
    build_route_table(&snapshot.routes, &mode_registry)
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

    fn build_table(raws: &[RawRouteSpec]) -> Result<RouteTable, Vec<RouteConfigError>> {
        let mode_registry = ResponseModeRegistry::with_builtins();
        build_route_table(raws, &mode_registry)
    }

    #[test]
    fn single_valid_route() {
        let raw = make_raw("r1", "/api/test", &["GET"], "none", "static");
        let table = build_table(&[raw]).unwrap();
        assert_eq!(table.all.len(), 1);
        let (route, caps) = table.match_request(&Method::GET, "/api/test").unwrap();
        assert_eq!(route.id, "r1");
        assert!(caps.is_empty());
    }

    #[test]
    fn duplicate_id_rejected() {
        let r1 = make_raw("r1", "/a", &["GET"], "none", "static");
        let r2 = make_raw("r1", "/b", &["GET"], "none", "static");
        let err = build_table(&[r1, r2]).unwrap_err();
        assert_eq!(err.len(), 1);
        let msg = format!("{}", err[0]);
        assert!(msg.contains("duplicate route id"), "got: {msg}");
    }

    #[test]
    fn reserved_path_prefix_rejected() {
        let raw = make_raw("r1", "/health", &["GET"], "none", "static");
        let err = build_table(&[raw]).unwrap_err();
        let msg = format!("{}", err[0]);
        assert!(msg.contains("reserved path"), "got: {msg}");
    }

    #[test]
    fn hook_prefix_allowed_for_config_routes() {
        let raw = make_raw("r1", "/hook/stripe", &["POST"], "none", "static");
        let table = build_table(&[raw])
            .expect("/hook/* must be admitted by the route table builder");
        assert_eq!(table.all.len(), 1);
    }

    #[test]
    fn fingerprint_is_deterministic() {
        let raw = make_raw("r1", "/a", &["GET"], "none", "static");
        let t1 = build_table(std::slice::from_ref(&raw)).unwrap();
        let t2 = build_table(&[raw]).unwrap();
        assert_eq!(t1.fingerprint, t2.fingerprint);
    }

    #[test]
    fn compiled_route_carries_semaphore_with_concurrent_max_permits() {
        let raw = make_raw("r1", "/api/test", &["GET"], "none", "static");
        let concurrent_max = raw.limits.concurrent_max;
        let table = build_table(&[raw]).unwrap();
        let route = &table.all[0];
        assert_eq!(route.semaphore.available_permits(), concurrent_max as usize);
    }
}
