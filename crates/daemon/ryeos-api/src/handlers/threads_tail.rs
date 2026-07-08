//! `threads.tail` — issue a descriptor for a thread's live event stream.
//!
//! This service does NOT stream bytes. It validates ownership and returns a
//! *stream descriptor* telling the client which signed SSE route to open. The
//! daemon mediates — it resolves the command to this item, authorizes, and hands
//! back a descriptor — but stays out of the byte path; the client opens the
//! stream itself. Any client (CLI, TUI, web) handles it identically, so this is
//! not CLI-specific.
//!
//! Ownership is enforced here at descriptor-issue time AND again by the SSE route
//! on open. Returns null for an unknown thread (like `threads.get`, to avoid
//! leaking existence).

use std::sync::Arc;

use anyhow::Result;
use serde_json::{json, Value};

use crate::handler_context::HandlerContext;
use crate::handler_error::HandlerError;
use crate::registry::ServiceDescriptor;
use ryeos_app::route_raw::RawRouteSpec;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

/// Canonical ids of the SSE routes the descriptor points at, defined in
/// `bundles/standard/.ai/node/routes/{thread-events-stream,chain-events-stream}.yaml`.
/// Referenced by id so each path template lives in exactly one place (its route).
const THREAD_EVENTS_ROUTE_ID: &str = "thread/events-stream";
const CHAIN_EVENTS_ROUTE_ID: &str = "chain/events-stream";

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub thread_id: String,
    /// Tail only this one thread and stop at its terminal, instead of following
    /// the whole braid (the thread's chain) across continuations. Tailing the
    /// braid is the default — it matches what the TUI does and never drops you
    /// when a thread continues. See the `follow` descriptor field the client
    /// honors (`chain` for the braid, `thread` for this single thread).
    #[serde(default)]
    pub thread_only: bool,
}

pub async fn handle(
    req: Request,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value, HandlerError> {
    let thread = state
        .threads
        .get_thread_view(&req.thread_id)
        .map_err(|e| HandlerError::Internal(e.to_string()))?;
    let Some(view) = thread else {
        // Mirror threads.get: null (not 403) for an unknown thread avoids
        // leaking existence to a non-owner.
        return Ok(Value::Null);
    };
    ctx.require_owner(view.thread.requested_by.as_deref())?;

    // Tailing the braid is the default: the chain stream spans continuations and
    // only ends on EOF, so it never drops the caller when a thread continues.
    // `thread_only` opts into the narrower stream, which follows just this one
    // thread and stops at its terminal. The `follow` field tells the client
    // which completion policy to apply.
    let (path, follow) = if req.thread_only {
        let path = resolve_stream_path(
            &state.node_config.routes,
            THREAD_EVENTS_ROUTE_ID,
            "thread_events",
            "thread_id",
            &req.thread_id,
        )?;
        (path, "thread")
    } else {
        let path = resolve_stream_path(
            &state.node_config.routes,
            CHAIN_EVENTS_ROUTE_ID,
            "chain_tail",
            "chain_root_id",
            &view.thread.chain_root_id,
        )?;
        (path, "chain")
    };
    Ok(json!({
        "stream": {
            "transport": "sse",
            "method": "GET",
            "path": path,
            "follow": follow,
        }
    }))
}

/// Look up an SSE route's path template by canonical id and fill its single
/// `{placeholder}` from `value`. Keeps the raw path out of this handler — it is
/// owned by the route descriptor. `expected_source` and `placeholder` pin which
/// route this is (thread-events vs chain-events) so a drifted/reused route id
/// can't produce a misleading descriptor.
fn resolve_stream_path(
    routes: &[RawRouteSpec],
    route_id: &str,
    expected_source: &str,
    placeholder: &str,
    value: &str,
) -> Result<String, HandlerError> {
    let route = routes.iter().find(|r| r.id == route_id).ok_or_else(|| {
        HandlerError::Internal(format!("stream route '{route_id}' is not registered"))
    })?;
    validate_stream_route_contract(route_id, route, expected_source, placeholder)?;
    let path = fill_path(&route.path, placeholder, value)?;
    // Belt-and-suspenders: the contract guarantees exactly one placeholder and
    // no others, so a filled path must carry no braces.
    if path.contains('{') || path.contains('}') {
        return Err(HandlerError::Internal(format!(
            "stream route '{route_id}' produced an unresolved path '{path}'"
        )));
    }
    Ok(path)
}

/// Verify the resolved route is still the kind of event-stream route we expect.
/// Guards against route drift or a mistakenly reused route id producing a
/// misleading descriptor. A mismatch is a server misconfiguration, hence
/// `Internal`.
fn validate_stream_route_contract(
    route_id: &str,
    route: &RawRouteSpec,
    expected_source: &str,
    placeholder: &str,
) -> Result<(), HandlerError> {
    let bad =
        |detail: String| HandlerError::Internal(format!("stream route '{route_id}' {detail}"));
    if !route.methods.iter().any(|m| m.eq_ignore_ascii_case("GET")) {
        return Err(bad("does not serve GET".to_string()));
    }
    if route.auth != "ryeos_signed" {
        return Err(bad(format!(
            "auth is '{}', expected ryeos_signed",
            route.auth
        )));
    }
    if route.response.mode != "event_stream" {
        return Err(bad(format!(
            "response mode is '{}', expected event_stream",
            route.response.mode
        )));
    }
    if route.response.source.as_deref() != Some(expected_source) {
        return Err(bad(format!(
            "response source is {:?}, expected {expected_source}",
            route.response.source
        )));
    }
    if !route.path.starts_with('/') || route.path.contains('?') || route.path.contains('#') {
        return Err(bad(format!(
            "path '{}' is not a clean node-relative path",
            route.path
        )));
    }
    let needle = format!("{{{placeholder}}}");
    if route.path.matches(&needle).count() != 1 {
        return Err(bad(format!(
            "path '{}' must contain exactly one {needle} placeholder",
            route.path
        )));
    }
    // No OTHER placeholders: removing the single one must leave none.
    let remainder = route.path.replacen(&needle, "", 1);
    if remainder.contains('{') || remainder.contains('}') {
        return Err(bad(format!(
            "path '{}' contains an unexpected placeholder",
            route.path
        )));
    }
    Ok(())
}

/// Interpolate `{placeholder}` into a route path template. The value is an
/// opaque id; reject anything with path-significant characters so the
/// interpolated path cannot be steered off-route (defense in depth — ownership
/// is already checked and the route re-checks auth on open).
fn fill_path(path_template: &str, placeholder: &str, value: &str) -> Result<String, HandlerError> {
    if value.is_empty()
        || !value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '~' | '-'))
    {
        return Err(HandlerError::BadRequest(format!(
            "invalid {placeholder} '{value}'"
        )));
    }
    Ok(path_template.replace(&format!("{{{placeholder}}}"), value))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:threads/tail",
    endpoint: "threads.tail",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, ctx, state).await.map_err(Into::into)
        })
    },
};

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_app::route_raw::RawResponseSpec;

    /// A route matching the expected `thread/events-stream` contract.
    fn valid_route() -> RawRouteSpec {
        RawRouteSpec {
            id: THREAD_EVENTS_ROUTE_ID.to_string(),
            path: "/threads/{thread_id}/events/stream".to_string(),
            methods: ["GET".to_string()].into_iter().collect(),
            auth: "ryeos_signed".to_string(),
            auth_config: None,
            limits: Default::default(),
            response: RawResponseSpec {
                mode: "event_stream".to_string(),
                source: Some("thread_events".to_string()),
                source_config: serde_json::Value::Null,
                status: None,
                content_type: None,
                body_b64: None,
            },
            execute: None,
            request: Default::default(),
            source_file: Default::default(),
        }
    }

    /// A route matching the expected `chain/events-stream` contract.
    fn valid_chain_route() -> RawRouteSpec {
        RawRouteSpec {
            id: CHAIN_EVENTS_ROUTE_ID.to_string(),
            path: "/chains/{chain_root_id}/events/stream".to_string(),
            methods: ["GET".to_string()].into_iter().collect(),
            auth: "ryeos_signed".to_string(),
            auth_config: None,
            limits: Default::default(),
            response: RawResponseSpec {
                mode: "event_stream".to_string(),
                source: Some("chain_tail".to_string()),
                source_config: serde_json::Value::Null,
                status: None,
                content_type: None,
                body_b64: None,
            },
            execute: None,
            request: Default::default(),
            source_file: Default::default(),
        }
    }

    /// Resolve via the thread-events contract (the non-braid path).
    fn resolve_thread(routes: &[RawRouteSpec], id: &str) -> Result<String, HandlerError> {
        resolve_stream_path(
            routes,
            THREAD_EVENTS_ROUTE_ID,
            "thread_events",
            "thread_id",
            id,
        )
    }

    #[test]
    fn fills_placeholder() {
        assert_eq!(
            fill_path(
                "/threads/{thread_id}/events/stream",
                "thread_id",
                "T-abc123"
            )
            .unwrap(),
            "/threads/T-abc123/events/stream"
        );
        assert_eq!(
            fill_path(
                "/chains/{chain_root_id}/events/stream",
                "chain_root_id",
                "T-root"
            )
            .unwrap(),
            "/chains/T-root/events/stream"
        );
    }

    #[test]
    fn rejects_path_significant_values() {
        for bad in ["", "a/b", "../etc", "a b", "a?x", "a#y"] {
            assert!(
                fill_path("/threads/{thread_id}/events/stream", "thread_id", bad).is_err(),
                "expected rejection for {bad:?}"
            );
        }
    }

    #[test]
    fn missing_route_is_internal_error() {
        assert!(matches!(
            resolve_thread(&[], "abc").unwrap_err(),
            HandlerError::Internal(_)
        ));
    }

    #[test]
    fn valid_thread_route_resolves() {
        assert_eq!(
            resolve_thread(&[valid_route()], "abc").unwrap(),
            "/threads/abc/events/stream"
        );
    }

    #[test]
    fn valid_chain_route_resolves() {
        assert_eq!(
            resolve_stream_path(
                &[valid_chain_route()],
                CHAIN_EVENTS_ROUTE_ID,
                "chain_tail",
                "chain_root_id",
                "T-root"
            )
            .unwrap(),
            "/chains/T-root/events/stream"
        );
    }

    #[test]
    fn route_source_mismatch_is_rejected() {
        // A thread-events route validated against the chain contract (wrong
        // source + placeholder) must fail — guards against route-id reuse.
        assert!(resolve_stream_path(
            &[valid_route()],
            THREAD_EVENTS_ROUTE_ID,
            "chain_tail",
            "chain_root_id",
            "abc"
        )
        .is_err());
    }

    #[test]
    fn route_contract_drift_is_rejected() {
        // Wrong auth.
        let mut r = valid_route();
        r.auth = "none".into();
        assert!(resolve_thread(&[r], "abc").is_err());

        // Wrong response mode.
        let mut r = valid_route();
        r.response.mode = "buffered".into();
        assert!(resolve_thread(&[r], "abc").is_err());

        // Wrong source.
        let mut r = valid_route();
        r.response.source = Some("dispatch_launch".into());
        assert!(resolve_thread(&[r], "abc").is_err());

        // Does not serve GET.
        let mut r = valid_route();
        r.methods = ["POST".to_string()].into_iter().collect();
        assert!(resolve_thread(&[r], "abc").is_err());

        // Unexpected extra placeholder.
        let mut r = valid_route();
        r.path = "/sites/{site_id}/threads/{thread_id}/events/stream".into();
        assert!(resolve_thread(&[r], "abc").is_err());

        // Missing the thread_id placeholder entirely.
        let mut r = valid_route();
        r.path = "/threads/events/stream".into();
        assert!(resolve_thread(&[r], "abc").is_err());

        // Duplicate {thread_id} — ambiguous, must be exactly one.
        let mut r = valid_route();
        r.path = "/threads/{thread_id}/{thread_id}/stream".into();
        assert!(resolve_thread(&[r], "abc").is_err());

        // Query string or fragment in the route path.
        let mut r = valid_route();
        r.path = "/threads/{thread_id}/events/stream?after=1".into();
        assert!(resolve_thread(&[r], "abc").is_err());
        let mut r = valid_route();
        r.path = "/threads/{thread_id}/events/stream#frag".into();
        assert!(resolve_thread(&[r], "abc").is_err());
    }
}
