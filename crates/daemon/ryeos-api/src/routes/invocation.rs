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

use crate::route_error::RouteDispatchError;
use ryeos_app::state::AppState;
use ryeos_app::stream_envelope::RouteStreamEnvelope;

// ── Principal ──────────────────────────────────────────────────────────────

/// Authenticated principal produced by auth verifiers, consumed by modes.
#[derive(Debug, Clone)]
pub struct RoutePrincipal {
    /// Stable identity string (e.g., `fp:<sha256>`, `webhook:hmac:<route_id>`).
    pub id: String,
    /// Granted capability scopes.
    pub scopes: Vec<String>,
    /// Which auth handler verified this (e.g., "none", "ryeos_signed", "hmac").
    pub verifier_key: &'static str,
    /// Whether the principal was cryptographically verified.
    pub verified: bool,
    /// Site identity cryptographically bound to a v2 remote-node grant.
    /// `None` for local clients and non-RyeOS verifiers. Execute routes must
    /// never populate this from request data.
    pub authenticated_origin_site_id: Option<String>,
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
            authenticated_origin_site_id: None,
            metadata: BTreeMap::new(),
        }
    }
}

/// Select execution origin exclusively from authenticated route authority.
/// Remote request payloads never participate; local and non-RyeOS principals
/// resolve to the serving node.
pub(crate) fn authenticated_execution_origin(
    principal: Option<&RoutePrincipal>,
    current_site_id: &str,
) -> String {
    principal
        .and_then(|value| value.authenticated_origin_site_id.clone())
        .unwrap_or_else(|| current_site_id.to_string())
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
    /// Optional request-owned execution workspace guard. Invokers attach it to
    /// execution provenance so blocking runtime work can outlive cancellation
    /// of the request future without losing its workspace.
    pub workspace_lifeline: Option<Arc<ryeos_app::temp_dir_guard::TempDirGuard>>,
    /// Shared daemon state.
    pub state: AppState,
    /// Process-wide webhook delivery-id dedupe store. Read by HMAC
    /// auth verifiers when a route configures dedupe. Plumbed here
    /// because it lives on `ApiState` (not `AppState`).
    pub webhook_dedupe: Arc<crate::routes::webhook_dedupe::WebhookDedupeStore>,
}

// ── Invocation result (output) ─────────────────────────────────────────────

/// Result of a route invocation.
///
/// Each variant corresponds to an invocation contract. The mode that
/// called the invoker expects a specific variant; receiving the wrong
/// one is an `Internal` error.
pub enum RouteInvocationResult {
    /// Synchronous JSON result (for `json` mode).
    Json {
        value: Value,
        /// Durable recorded-service identity exposed as
        /// `x-ryeos-thread-id` without changing the authored JSON body.
        thread_id: Option<String>,
    },
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
            Self::Json { .. } => "Json",
            Self::Stream(_) => "Stream",
            Self::Principal(_) => "Principal",
            Self::Accepted { .. } => "Accepted",
        }
    }
}

/// Attach the durable recorded-service identity without changing the authored
/// response body. Every JSON-producing response mode uses this one framing
/// boundary so recorded and unrecorded behavior cannot drift by mode.
pub(crate) fn attach_recorded_thread_header(
    response: &mut axum::response::Response,
    thread_id: Option<&str>,
) -> Result<(), RouteDispatchError> {
    if let Some(thread_id) = thread_id {
        response.headers_mut().insert(
            "x-ryeos-thread-id",
            axum::http::HeaderValue::from_str(thread_id).map_err(|error| {
                RouteDispatchError::Internal(format!(
                    "recorded service produced an invalid thread id header: {error}"
                ))
            })?,
        );
    }
    Ok(())
}

/// Envelope stream returned by streaming invokers.
///
/// All streaming invokers emit transport-neutral `RouteStreamEnvelope` values.
/// The SSE transport layer in `event_stream_mode` converts envelopes to SSE frames.
pub struct RouteEventStream {
    /// Envelope stream producing `RouteStreamEnvelope` items.
    pub events:
        Pin<Box<dyn Stream<Item = Result<RouteStreamEnvelope, std::convert::Infallible>> + Send>>,
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

// ── Contract enforcement ──────────────────────────────────────────────────

/// Expected contract for an invocation call.
///
/// Modes pass this to `invoke_checked` so the enforcement layer can verify
/// the invoker's declared contract matches what the mode expects before
/// calling it, and verify the result type matches after.
pub struct InvocationCheck {
    /// What `RouteInvocationOutput` the mode expects the invoker to produce.
    pub expected_output: RouteInvocationOutput,
}

/// Central contract enforcement for all route invocation.
///
/// 1. Verifies the invoker's declared output matches what the mode expects.
/// 2. Enforces `PrincipalPolicy` against the context's principal.
/// 3. Calls the invoker.
/// 4. Verifies the returned result type matches the declared contract.
///
/// Every mode should use this instead of calling `invoker.invoke()` directly.
pub async fn invoke_checked(
    invoker: &dyn CompiledRouteInvocation,
    check: InvocationCheck,
    ctx: RouteInvocationContext,
) -> Result<RouteInvocationResult, RouteDispatchError> {
    let contract = invoker.contract();

    // 1. Verify output contract matches mode expectation.
    if contract.output != check.expected_output {
        return Err(RouteDispatchError::Internal(format!(
            "contract mismatch: mode expects {:?}, invoker declares {:?}",
            check.expected_output, contract.output
        )));
    }

    // 2. Enforce principal policy.
    let has_principal = ctx.principal.is_some();
    match contract.principal {
        PrincipalPolicy::Forbidden => {
            if has_principal {
                return Err(RouteDispatchError::Internal(
                    "invoker forbids principal but context has one".into(),
                ));
            }
        }
        PrincipalPolicy::Optional => { /* no requirement */ }
        PrincipalPolicy::Required => {
            if !has_principal {
                return Err(RouteDispatchError::Unauthorized);
            }
        }
    }

    // 3. Call invoker.
    let result = invoker.invoke(ctx).await?;

    // 4. Verify result type matches declared contract.
    let result_matches = matches!(
        (&result, contract.output),
        (
            RouteInvocationResult::Json { .. },
            RouteInvocationOutput::Json,
        ) | (
            RouteInvocationResult::Stream(_),
            RouteInvocationOutput::Stream
        ) | (
            RouteInvocationResult::Principal(_),
            RouteInvocationOutput::Principal
        ) | (
            RouteInvocationResult::Accepted { .. },
            RouteInvocationOutput::Accepted
        )
    );

    if !result_matches {
        return Err(RouteDispatchError::Internal(format!(
            "invoker returned {} but contract declares {:?}",
            result.variant_name(),
            contract.output
        )));
    }

    Ok(result)
}

#[cfg(test)]
mod origin_tests {
    use axum::response::IntoResponse;

    use super::*;

    #[test]
    fn authenticated_remote_site_is_the_execution_origin() {
        let mut principal = RoutePrincipal::anonymous("fp:remote".to_string(), "ryeos_signed");
        principal.authenticated_origin_site_id = Some("site:remote".to_string());
        assert_eq!(
            authenticated_execution_origin(Some(&principal), "site:local"),
            "site:remote"
        );
    }

    #[test]
    fn local_principal_uses_the_serving_site_as_origin() {
        let principal = RoutePrincipal::anonymous("fp:local".to_string(), "ryeos_signed");
        assert_eq!(
            authenticated_execution_origin(Some(&principal), "site:local"),
            "site:local"
        );
        assert_eq!(
            authenticated_execution_origin(None, "site:local"),
            "site:local"
        );
    }

    #[test]
    fn recorded_thread_header_is_present_only_for_recorded_results() {
        let mut recorded = axum::Json(serde_json::json!({"ok": true})).into_response();
        attach_recorded_thread_header(&mut recorded, Some("svc-test")).unwrap();
        assert_eq!(
            recorded.headers().get("x-ryeos-thread-id").unwrap(),
            "svc-test"
        );

        let mut unrecorded = axum::Json(serde_json::json!({"ok": true})).into_response();
        attach_recorded_thread_header(&mut unrecorded, None).unwrap();
        assert!(unrecorded.headers().get("x-ryeos-thread-id").is_none());
    }
}
