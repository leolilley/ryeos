use std::collections::HashMap;
use std::sync::Arc;

use axum::http::Method;

use super::compile::CompiledRoute;

#[derive(Debug)]
enum Segment {
    Literal(String),
    Capture(String),
}

struct MatchEntry {
    segments: Vec<Segment>,
    route: Arc<CompiledRoute>,
}

impl std::fmt::Debug for MatchEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MatchEntry")
            .field("route_id", &self.route.id)
            .finish()
    }
}

#[derive(Debug)]
pub struct PathMatcher {
    entries: Vec<MatchEntry>,
}

impl PathMatcher {
    pub fn new(routes: Vec<Arc<CompiledRoute>>) -> Result<Self, crate::dispatch_error::RouteConfigError> {
        let mut entries = Vec::new();
        for route in routes {
            let segments = parse_path(&route.path_pattern, &route.id)?;
            entries.push(MatchEntry {
                segments,
                route: route.clone(),
            });
        }

        for i in 0..entries.len() {
            for j in (i + 1)..entries.len() {
                if !patterns_can_collide(&entries[i].segments, &entries[j].segments) {
                    continue;
                }
                let shared_methods: Vec<&Method> = entries[i].route.methods.iter()
                    .filter(|m| entries[j].route.methods.contains(m))
                    .collect();
                if let Some(method) = shared_methods.into_iter().next() {
                    return Err(crate::dispatch_error::RouteConfigError::PathCollision {
                        id_a: entries[i].route.id.clone(),
                        id_b: entries[j].route.id.clone(),
                        pattern: entries[i].route.path_pattern.clone(),
                        method: method.to_string(),
                    });
                }
            }
        }

        Ok(Self { entries })
    }

    pub fn match_request(
        &self,
        method: &Method,
        path: &str,
    ) -> Option<(Arc<CompiledRoute>, HashMap<String, String>)> {
        let path_segments: Vec<&str> = path
            .split('/')
            .filter(|s| !s.is_empty())
            .collect();

        for entry in &self.entries {
            if !entry.route.methods.iter().any(|m| m == method) {
                continue;
            }
            if path_segments.len() != entry.segments.len() {
                continue;
            }

            let mut captures = HashMap::new();
            let mut matched = true;
            for (path_seg, template_seg) in path_segments.iter().zip(entry.segments.iter()) {
                match template_seg {
                    Segment::Literal(lit) => {
                        if *path_seg != lit {
                            matched = false;
                            break;
                        }
                    }
                    Segment::Capture(name) => {
                        captures.insert(name.clone(), (*path_seg).to_string());
                    }
                }
            }

            if matched {
                return Some((entry.route.clone(), captures));
            }
        }
        None
    }
}

fn patterns_can_collide(a: &[Segment], b: &[Segment]) -> bool {
    if a.len() != b.len() { return false; }
    for (sa, sb) in a.iter().zip(b) {
        match (sa, sb) {
            (Segment::Literal(la), Segment::Literal(lb)) if la != lb => return false,
            _ => {}
        }
    }
    true
}

fn parse_path(
    pattern: &str,
    route_id: &str,
) -> Result<Vec<Segment>, crate::dispatch_error::RouteConfigError> {
    let mut segments = Vec::new();
    let parts: Vec<&str> = pattern.split('/').filter(|s| !s.is_empty()).collect();

    for part in parts {
        if let Some(name) = part.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
            if name.is_empty() {
                return Err(crate::dispatch_error::RouteConfigError::InvalidPathTemplate {
                    id: route_id.to_string(),
                    reason: "empty capture name".to_string(),
                });
            }
            if name.contains('{') || name.contains('}') {
                return Err(crate::dispatch_error::RouteConfigError::InvalidPathTemplate {
                    id: route_id.to_string(),
                    reason: format!("invalid capture name '{name}'"),
                });
            }
            segments.push(Segment::Capture(name.to_string()));
        } else {
            segments.push(Segment::Literal(part.to_string()));
        }
    }

    Ok(segments)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_route(id: &str, path: &str, methods: &[&str]) -> Arc<CompiledRoute> {
        use crate::routes::compile::{AuthVerifier, ResponseMode};
        use crate::routes::raw::{RawLimits, RawRequest, RawRequestBody, RawResponseSpec, RawRouteSpec};
        use crate::routes::response_modes::static_mode::StaticMode;

        // Use `static` mode for the matcher's unit tests: it's the
        // simplest registered mode whose compile-path produces a
        // valid `CompiledResponseMode` from a minimal `RawRouteSpec`.
        // The test only exercises path/method matching — it never
        // dispatches the route — so the response body is irrelevant.
        let mode = StaticMode;
        let ctx = crate::routes::compile::ModeCompileContext {
            _phantom: std::marker::PhantomData,
        };
        let raw = RawRouteSpec {
            section: "routes".to_string(),
            id: id.to_string(),
            path: path.to_string(),
            methods: methods.iter().map(|s| s.to_string()).collect(),
            auth: "none".to_string(),
            auth_config: None,
            limits: RawLimits::default(),
            response: RawResponseSpec {
                mode: "static".to_string(),
                source: None,
                source_config: serde_json::Value::Null,
                status: Some(200),
                content_type: Some("text/plain".to_string()),
                body_b64: Some("aGVsbG8=".to_string()), // "hello"
            },
            execute: None,
            request: RawRequest { body: RawRequestBody::None },
            source_file: std::path::PathBuf::new(),
        };
        Arc::new(CompiledRoute {
            id: id.to_string(),
            source_file: raw.source_file.clone(),
            path_pattern: path.to_string(),
            methods: methods.iter().map(|s| s.parse().unwrap()).collect(),
            auth: crate::routes::verifiers::none::NoneVerifier
                .validate_route_config(id, None)
                .unwrap(),
            limits: crate::routes::compile::CompiledLimits {
                body_bytes_max: 1048576,
                timeout_ms: 30000,
                concurrent_max: 100,
            },
            response_mode: mode.compile(&raw, &ctx).unwrap(),
            raw_response: raw,
            semaphore: std::sync::Arc::new(tokio::sync::Semaphore::new(100)),
        })
    }

    #[test]
    fn exact_match() {
        let routes = vec![make_route("r1", "/health", &["GET"])];
        let matcher = PathMatcher::new(routes).unwrap();
        let (route, caps) = matcher
            .match_request(&Method::GET, "/health")
            .unwrap();
        assert_eq!(route.id, "r1");
        assert!(caps.is_empty());
    }

    #[test]
    fn no_match_wrong_path() {
        let routes = vec![make_route("r1", "/health", &["GET"])];
        let matcher = PathMatcher::new(routes).unwrap();
        assert!(matcher.match_request(&Method::GET, "/foo").is_none());
    }

    #[test]
    fn no_match_wrong_method() {
        let routes = vec![make_route("r1", "/health", &["GET"])];
        let matcher = PathMatcher::new(routes).unwrap();
        assert!(matcher.match_request(&Method::POST, "/health").is_none());
    }

    #[test]
    fn capture_match() {
        let routes = vec![make_route("r1", "/users/{id}", &["GET"])];
        let matcher = PathMatcher::new(routes).unwrap();
        let (route, caps) = matcher
            .match_request(&Method::GET, "/users/42")
            .unwrap();
        assert_eq!(route.id, "r1");
        assert_eq!(caps.get("id").unwrap(), "42");
    }

    #[test]
    fn capture_multiple() {
        let routes = vec![make_route("r1", "/users/{user_id}/posts/{post_id}", &["GET"])];
        let matcher = PathMatcher::new(routes).unwrap();
        let (_, caps) = matcher
            .match_request(&Method::GET, "/users/u1/posts/p2")
            .unwrap();
        assert_eq!(caps.get("user_id").unwrap(), "u1");
        assert_eq!(caps.get("post_id").unwrap(), "p2");
    }

    #[test]
    fn capture_wrong_segment_count() {
        let routes = vec![make_route("r1", "/users/{id}", &["GET"])];
        let matcher = PathMatcher::new(routes).unwrap();
        assert!(matcher
            .match_request(&Method::GET, "/users/42/extra")
            .is_none());
    }

    #[test]
    fn empty_capture_name_rejected() {
        let err = parse_path("/{}/foo", "r1").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("empty capture name"), "got: {msg}");
    }

    #[test]
    fn path_collision_detected() {
        let routes = vec![
            make_route("r1", "/items/{id}", &["GET"]),
            make_route("r2", "/items/{id}", &["GET"]),
        ];
        let result = PathMatcher::new(routes);
        let err = result.unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("path collision"), "got: {msg}");
    }

    #[test]
    fn different_literal_paths_no_collision() {
        let routes = vec![
            make_route("r1", "/users/{id}", &["GET"]),
            make_route("r2", "/items/{id}", &["GET"]),
        ];
        assert!(PathMatcher::new(routes).is_ok());
    }

    #[test]
    fn same_capture_path_different_methods_no_collision() {
        let routes = vec![
            make_route("r1", "/items/{id}", &["GET"]),
            make_route("r2", "/items/{id}", &["DELETE"]),
        ];
        assert!(PathMatcher::new(routes).is_ok());
    }
}
