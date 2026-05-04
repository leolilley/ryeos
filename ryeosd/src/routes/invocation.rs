//! Route invocation protocol — one runtime contract for all route dispatch.
//!
//! `CompiledRouteInvocation` is the single trait that replaces:
//! - `CompiledAuthVerifier` (auth stage)
//! - Inline service dispatch in `json_mode`
//! - Inline subscription logic in `event_stream_mode`
//! - The launch helper in `launch_mode`
//!
//! Every route source becomes a compiled invoker at route-table-build time.
//! Modes become pure HTTP framing — they construct a `RouteInvocationContext`,
//! call their invoker, and frame the `RouteInvocationResult` for HTTP.

use std::collections::BTreeMap;
use std::pin::Pin;
use std::sync::Arc;

use axum::http::{HeaderMap, Method, Uri};
use serde_json::Value;
use tokio_stream::Stream;

use crate::dispatch_error::RouteDispatchError;
use crate::state::AppState;

// ── Principal ──────────────────────────────────────────────────────────────

/// Authenticated principal produced by auth verifiers, consumed by modes.
#[derive(Debug, Clone)]
pub struct RoutePrincipal {
    /// Stable identity string (e.g., `fp:<sha256>`, `webhook:hmac:<route_id>`).
    pub id: String,
    /// Granted capability scopes.
    pub scopes: Vec<String>,
    /// Which auth handler verified this (e.g., "none", "rye_signed", "hmac").
    pub verifier_key: &'static str,
    /// Whether the principal was cryptographically verified.
    pub verified: bool,
    /// Verifier-supplied metadata for downstream consumption.
    ///
    /// `BTreeMap` for deterministic JSON serialization order.
    pub metadata: BTreeMap<String, String>,
}

impl RoutePrincipal {
    /// Convenience constructor with empty scopes + metadata.
    pub fn anonymous(id: String, verifier_key: &'static str) -> Self {
        Self {
            id,
            scopes: Vec::new(),
            verifier_key,
            verified: false,
            metadata: BTreeMap::new(),
        }
    }
}

// ── Invocation context (input) ─────────────────────────────────────────────

/// Complete request context available to compiled invokers.
///
/// Constructed at dispatch time and passed to whatever compiled invoker
/// the route resolved to. Carries everything a handler might need.
pub struct RouteInvocationContext {
    /// Route ID (for audit/logging).
    pub route_id: Arc<str>,
    /// HTTP method.
    pub method: Method,
    /// Request URI.
    pub uri: Uri,
    /// Path template captures (e.g., `{"thread_id": "abc-123"}`).
    pub captures: BTreeMap<String, String>,
    /// HTTP request headers.
    pub headers: HeaderMap,
    /// Raw request body bytes.
    pub body_raw: Vec<u8>,
    /// Prepared invocation payload:
    /// - json mode: interpolated source_config
    /// - event_stream subscription: interpolated source_config
    /// - auth verifiers: ignored (Value::Null)
    /// - gateway launch: parsed request body
    /// - launch mode: launch envelope
    pub input: Value,
    /// Authenticated principal.
    /// `None` during auth verification; `Some(_)` during response handling.
    pub principal: Option<RoutePrincipal>,
    /// Shared daemon state.
    pub state: AppState,
}

// ── Invocation result (output) ─────────────────────────────────────────────

/// Result of a route invocation.
///
/// Each variant corresponds to an invocation contract. The mode that
/// called the invoker expects a specific variant; receiving the wrong
/// one is an `Internal` error.
pub enum RouteInvocationResult {
    /// Synchronous JSON result (for `json` mode).
    Json(Value),
    /// SSE event stream (for `event_stream` mode).
    Stream(RouteEventStream),
    /// Auth principal (for auth verifiers, never seen by modes).
    Principal(RoutePrincipal),
    /// Accepted with thread ID (for `launch`/`accepted` mode).
    Accepted { thread_id: String },
}

impl RouteInvocationResult {
    /// Debug name for error messages.
    pub fn variant_name(&self) -> &'static str {
        match self {
            Self::Json(_) => "Json",
            Self::Stream(_) => "Stream",
            Self::Principal(_) => "Principal",
            Self::Accepted { .. } => "Accepted",
        }
    }
}

/// SSE event stream returned by streaming invokers.
pub struct RouteEventStream {
    /// SSE event stream producing `axum::response::sse::Event` items.
    pub events: Pin<
        Box<
            dyn Stream<
                    Item = Result<axum::response::sse::Event, std::convert::Infallible>,
                > + Send,
        >,
    >,
    /// Keep-alive interval in seconds.
    pub keep_alive_secs: u64,
}

// ── Contract ───────────────────────────────────────────────────────────────

/// Static contract describing what an invoker produces and how it uses auth.
///
/// Compile-time constant per invoker. The dispatcher can verify expectations
/// (auth must produce `Principal`, json mode expects `Json`, etc.) without
/// needing a type enum.
pub struct RouteInvocationContract {
    /// What kind of result this invoker produces.
    pub output: RouteInvocationOutput,
    /// How this invoker interacts with auth principals.
    pub principal: PrincipalPolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteInvocationOutput {
    /// Service handlers, data sources → `RouteInvocationResult::Json`.
    Json,
    /// Streaming subscriptions, gateway launch → `RouteInvocationResult::Stream`.
    Stream,
    /// Auth verifiers → `RouteInvocationResult::Principal`.
    Principal,
    /// Launch/accepted → `RouteInvocationResult::Accepted`.
    Accepted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrincipalPolicy {
    /// Auth verifiers: principal must be `None` (auth creates the principal).
    Forbidden,
    /// Most read operations: principal if verified, `None` if anonymous.
    Optional,
    /// Ownership-checked operations: must have a principal.
    Required,
}

// ── The trait ──────────────────────────────────────────────────────────────

/// The single runtime contract for all route invocation.
///
/// Every route source (service handlers, auth verifiers, streaming
/// subscriptions, gateway launches) implements this trait. Modes construct
/// a `RouteInvocationContext`, call `invoke`, and frame the result for HTTP.
#[axum::async_trait]
pub trait CompiledRouteInvocation: Send + Sync {
    /// Static contract — what this invoker produces.
    fn contract(&self) -> &'static RouteInvocationContract;

    /// Execute the invocation.
    async fn invoke(
        &self,
        ctx: RouteInvocationContext,
    ) -> Result<RouteInvocationResult, RouteDispatchError>;
}
