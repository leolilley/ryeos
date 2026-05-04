//! Route invocation protocol — unified contract for route→handler dispatch.
//!
//! Phase 1 provides the `Json` variant for synchronous service calls.
//! Phase 2 adds `Stream` for SSE subscriptions. Phase 3 adds `Principal`
//! for auth verifiers.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::Value;

use crate::state::AppState;

/// Complete request context available to route handlers.
///
/// Constructed at dispatch time (after auth verification) and passed to
/// whatever backend the route's canonical ref resolves to. Carries
/// everything a handler might need — no partial states.
pub struct RouteInvocationContext {
    /// Path template captures (e.g., `{"thread_id": "abc-123"}`).
    pub captures: HashMap<String, String>,
    /// HTTP request headers.
    pub headers: Vec<(String, Vec<u8>)>,
    /// Raw request body bytes.
    pub body_raw: Option<Vec<u8>>,
    /// Authenticated principal (set by the verifier).
    pub principal: Option<RoutePrincipal>,
    /// Route-level auth config (from YAML `auth_config`).
    pub auth_config: Option<Value>,
    /// Shared daemon state.
    pub state: Arc<AppState>,
}

/// Authenticated principal produced by the verifier.
#[derive(Debug, Clone)]
pub struct RoutePrincipal {
    /// Stable identity string (e.g., `fp:<sha256>`, `webhook:hmac:<route_id>`).
    pub id: String,
    /// Granted capability scopes.
    pub scopes: Vec<String>,
    /// Verifier-supplied metadata (delivery_id, headers, etc.).
    pub metadata: HashMap<String, String>,
}

/// Result of a route handler invocation.
///
/// Each variant corresponds to a response mode. Handlers return the
/// variant appropriate to their `ServiceHandler` type; the mode wraps
/// it for HTTP.
pub enum RouteInvocationResult {
    /// Synchronous JSON result (for `json` mode, `ServiceHandler::Sync`).
    Json(Value),
    // Phase 2: Stream(SseEventStream)
    // Phase 3: Principal(RoutePrincipal)
}
