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

/// Canonical id of the SSE route the descriptor points at, defined in
/// `bundles/standard/.ai/node/routes/thread-events-stream.yaml`. Referenced by
/// id so the path template lives in exactly one place (the route descriptor).
const THREAD_EVENTS_ROUTE_ID: &str = "thread/events-stream";

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub thread_id: String,
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

    let path = resolve_stream_path(&state.node_config.routes, THREAD_EVENTS_ROUTE_ID, &req.thread_id)?;
    Ok(json!({
        "stream": {
            "transport": "sse",
            "method": "GET",
            "path": path,
        }
    }))
}

/// Look up the SSE route's path template by canonical id and fill its
/// `{thread_id}` placeholder. Keeps the raw path out of this handler — it is
/// owned by the route descriptor.
fn resolve_stream_path(
    routes: &[RawRouteSpec],
    route_id: &str,
    thread_id: &str,
) -> Result<String, HandlerError> {
    let route = routes
        .iter()
        .find(|r| r.id == route_id)
        .ok_or_else(|| HandlerError::Internal(format!("stream route '{route_id}' is not registered")))?;
    validate_stream_route_contract(route_id, route)?;
    let path = fill_thread_path(&route.path, thread_id)?;
    // Belt-and-suspenders: the contract guarantees exactly one `{thread_id}` and
    // no other placeholders, so a filled path must carry no braces.
    if path.contains('{') || path.contains('}') {
        return Err(HandlerError::Internal(format!(
            "stream route '{route_id}' produced an unresolved path '{path}'"
        )));
    }
    Ok(path)
}

/// Verify the resolved route is still the kind of route we expect to hand a
/// client as a thread event stream. Guards against route drift or a mistakenly
/// reused route id producing a misleading descriptor. A mismatch is a server
/// misconfiguration, hence `Internal`.
fn validate_stream_route_contract(route_id: &str, route: &RawRouteSpec) -> Result<(), HandlerError> {
    let bad = |detail: String| HandlerError::Internal(format!("stream route '{route_id}' {detail}"));
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
    if route.response.source.as_deref() != Some("thread_events") {
        return Err(bad(format!(
            "response source is {:?}, expected thread_events",
            route.response.source
        )));
    }
    if !route.path.starts_with('/') || route.path.contains('?') || route.path.contains('#') {
        return Err(bad(format!(
            "path '{}' is not a clean node-relative path",
            route.path
        )));
    }
    if route.path.matches("{thread_id}").count() != 1 {
        return Err(bad(format!(
            "path '{}' must contain exactly one {{thread_id}} placeholder",
            route.path
        )));
    }
    // No OTHER placeholders: removing the single `{thread_id}` must leave none.
    let remainder = route.path.replacen("{thread_id}", "", 1);
    if remainder.contains('{') || remainder.contains('}') {
        return Err(bad(format!(
            "path '{}' contains an unexpected placeholder",
            route.path
        )));
    }
    Ok(())
}

/// Interpolate `{thread_id}` into a route path template. The thread id is an
/// opaque token; reject anything with path-significant characters so the
/// interpolated path cannot be steered off-route (defense in depth — ownership
/// is already checked and the route re-checks auth on open).
fn fill_thread_path(path_template: &str, thread_id: &str) -> Result<String, HandlerError> {
    if thread_id.is_empty()
        || !thread_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '~' | '-'))
    {
        return Err(HandlerError::BadRequest(format!(
            "invalid thread_id '{thread_id}'"
        )));
    }
    Ok(path_template.replace("{thread_id}", thread_id))
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

    #[test]
    fn fills_thread_id_placeholder() {
        assert_eq!(
            fill_thread_path("/threads/{thread_id}/events/stream", "T-abc123").unwrap(),
            "/threads/T-abc123/events/stream"
        );
    }

    #[test]
    fn rejects_path_significant_thread_ids() {
        for bad in ["", "a/b", "../etc", "a b", "a?x", "a#y"] {
            assert!(
                fill_thread_path("/threads/{thread_id}/events/stream", bad).is_err(),
                "expected rejection for {bad:?}"
            );
        }
    }

    #[test]
    fn missing_route_is_internal_error() {
        let err = resolve_stream_path(&[], THREAD_EVENTS_ROUTE_ID, "abc").unwrap_err();
        assert!(matches!(err, HandlerError::Internal(_)));
    }

    #[test]
    fn valid_route_resolves_to_interpolated_path() {
        let routes = vec![valid_route()];
        assert_eq!(
            resolve_stream_path(&routes, THREAD_EVENTS_ROUTE_ID, "abc").unwrap(),
            "/threads/abc/events/stream"
        );
    }

    #[test]
    fn route_contract_drift_is_rejected() {
        // Wrong auth.
        let mut r = valid_route();
        r.auth = "none".into();
        assert!(resolve_stream_path(&[r], THREAD_EVENTS_ROUTE_ID, "abc").is_err());

        // Wrong response mode.
        let mut r = valid_route();
        r.response.mode = "buffered".into();
        assert!(resolve_stream_path(&[r], THREAD_EVENTS_ROUTE_ID, "abc").is_err());

        // Wrong source.
        let mut r = valid_route();
        r.response.source = Some("dispatch_launch".into());
        assert!(resolve_stream_path(&[r], THREAD_EVENTS_ROUTE_ID, "abc").is_err());

        // Does not serve GET.
        let mut r = valid_route();
        r.methods = ["POST".to_string()].into_iter().collect();
        assert!(resolve_stream_path(&[r], THREAD_EVENTS_ROUTE_ID, "abc").is_err());

        // Unexpected extra placeholder.
        let mut r = valid_route();
        r.path = "/sites/{site_id}/threads/{thread_id}/events/stream".into();
        assert!(resolve_stream_path(&[r], THREAD_EVENTS_ROUTE_ID, "abc").is_err());

        // Missing the thread_id placeholder entirely.
        let mut r = valid_route();
        r.path = "/threads/events/stream".into();
        assert!(resolve_stream_path(&[r], THREAD_EVENTS_ROUTE_ID, "abc").is_err());

        // Duplicate {thread_id} — ambiguous, must be exactly one.
        let mut r = valid_route();
        r.path = "/threads/{thread_id}/{thread_id}/stream".into();
        assert!(resolve_stream_path(&[r], THREAD_EVENTS_ROUTE_ID, "abc").is_err());

        // Query string or fragment in the route path.
        let mut r = valid_route();
        r.path = "/threads/{thread_id}/events/stream?after=1".into();
        assert!(resolve_stream_path(&[r], THREAD_EVENTS_ROUTE_ID, "abc").is_err());
        let mut r = valid_route();
        r.path = "/threads/{thread_id}/events/stream#frag".into();
        assert!(resolve_stream_path(&[r], THREAD_EVENTS_ROUTE_ID, "abc").is_err());
    }
}
