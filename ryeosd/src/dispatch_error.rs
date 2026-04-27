//! Typed dispatch errors with explicit HTTP status mapping.
//!
//! Replaces the V5.2/V5.3-pre substring HTTP mapping that used to live
//! in `api/execute.rs` (`msg.contains("insufficient capabilities")`,
//! `msg.contains("push first")`, `msg.contains("is not root-executable")`).
//! Each enumerated variant carries the structured fields callers need
//! to reason about the failure, plus a `http_status()` method that
//! `api/execute.rs` consults exactly once per request.
//!
//! The variant names — and the `http_status()` arms — are the source
//! of truth for `/execute` non-200 surfaces. The pin tests in
//! `ryeosd/tests/dispatch_pin.rs` lock the resulting status codes and
//! JSON shapes; if a future variant changes the status mapping, the
//! pin test catches it before the HTTP contract drifts.
//!
//! **V5.4 P1.2**: operator-fixable failures are now distinct variants
//! instead of collapsing into `Internal(#[from] anyhow::Error) → 500`.
//! The HTTP contract is honest: cap denial → 403, manifest miss → 502,
//! push-first → 409, unknown service handler → 502, materialization
//! error → 502. Only truly unexpected internal errors remain 500.

use axum::http::StatusCode;

#[derive(thiserror::Error, Debug)]
pub enum DispatchError {
    #[error("invalid item ref '{0}': {1}")]
    InvalidRef(String, String),
    #[error("kind '{kind}' is not root-executable: {detail}")]
    NotRootExecutable { kind: String, detail: String },
    #[error("insufficient capabilities for runtime '{runtime}': required {required:?}, caller_scopes {caller_scopes:?}")]
    InsufficientCaps {
        runtime: String,
        required: Vec<String>,
        caller_scopes: Vec<String>,
    },
    #[error("alias cycle detected resolving '{root_ref}': visited {visited:?}")]
    AliasCycle {
        root_ref: String,
        visited: Vec<String>,
    },
    #[error("alias chain exceeded MAX_HOPS ({max_hops}) resolving '{root_ref}'")]
    AliasChainTooLong {
        root_ref: String,
        max_hops: usize,
    },
    #[error("schema misconfigured for kind '{kind}': {detail}")]
    SchemaMisconfigured { kind: String, detail: String },
    /// `Display` is the bare reason — pin tests assert byte-equality
    /// of the wording (`"detached mode not yet supported for native runtimes"`,
    /// etc.). The variant name carries the diagnostic context; the
    /// surface string preserves the V5.2 contract.
    #[error("{reason}")]
    CapabilityRejected { reason: String },
    #[error("streaming dispatch outcome is not implemented in V5.3")]
    StreamingNotImplemented,
    #[error("project source error: {0}")]
    ProjectSource(String),
    // ── P1.2: operator-fixable failures, no longer 500 ────────────
    /// Service handler not found in the in-process handler registry.
    /// The kind schema declared `InProcessHandler { Services }` but
    /// no handler matched the item's service name.
    #[error("service handler not found for '{service_ref}' in {registry}; available: [{available}]")]
    ServiceHandlerMissing {
        service_ref: String,
        registry: String,
        available: String,
    },
    /// Service capability denied — the caller lacks the scope required
    /// by the service handler's declared capabilities.
    #[error("service '{service_ref}' denied: caller scopes {caller_scopes:?} do not include required '{required}'")]
    ServiceCapDenied {
        service_ref: String,
        required: String,
        caller_scopes: Vec<String>,
    },
    /// Service is unavailable in the current execution mode (e.g.
    /// daemon-only service called from offline/standalone mode).
    #[error("service '{service_ref}' is unavailable in {mode} mode (requires {requires})")]
    ServiceUnavailable {
        service_ref: String,
        mode: String,
        requires: String,
    },
    /// Subprocess executor missing — the resolved item's executor_ref
    /// does not correspond to a known executor.
    #[error("subprocess executor missing for '{item_ref}': {detail}")]
    SubprocessExecutorMissing { item_ref: String, detail: String },
    /// Subprocess run failed — the inline or detached run encountered
    /// an error after resolution succeeded.
    #[error("subprocess run failed for '{item_ref}': {detail}")]
    SubprocessRunFailed { item_ref: String, detail: String },
    /// Runtime binary materialization failed — the native executor
    /// could not be resolved from the bundle CAS.
    #[error("runtime materialization failed for '{executor_ref}': {detail}")]
    RuntimeMaterializationFailed { executor_ref: String, detail: String },
    /// Project source push-first — the project has not been pushed to
    /// the daemon's CAS before execution was requested. The Display
    /// is the bare wording (e.g. `"no pushed HEAD for project '<path>' \
    /// — push first"`) so the V5.2 pin in `dispatch_pin.rs::\
    /// pin_native_runtime_with_pushed_head` continues to hold byte-\
    /// identically. The HTTP layer maps this variant to 409.
    #[error("{0}")]
    ProjectSourcePushFirst(String),
    /// Project source checkout failed — the pushed HEAD snapshot
    /// could not be checked out from CAS.
    #[error("project source checkout failed: {0}")]
    ProjectSourceCheckoutFailed(String),
    #[error("internal: {0}")]
    Internal(#[from] anyhow::Error),
}

// ── Route-config specific errors ────────────────────────────────────────

#[derive(thiserror::Error, Debug)]
pub enum RouteConfigError {
    #[error("duplicate route id '{id}': first at {first_source}, second at {second_source}")]
    DuplicateRouteId {
        id: String,
        first_source: String,
        second_source: String,
    },
    #[error("invalid methods for route '{id}': {reason}")]
    InvalidMethods { id: String, reason: String },
    #[error("route '{id}' uses reserved path '{path}' (conflicts with '{reserved}')")]
    ReservedPathPrefix {
        id: String,
        path: String,
        reserved: String,
    },
    #[error("unknown auth verifier '{name}' for route '{id}'")]
    UnknownVerifier { id: String, name: String },
    #[error("unknown response mode '{name}' for route '{id}'")]
    UnknownResponseMode { id: String, name: String },
    #[error("invalid limits for route '{id}': {reason}")]
    InvalidLimits { id: String, reason: String },
    #[error("path collision between routes '{id_a}' and '{id_b}' on pattern '{pattern}' method {method}")]
    PathCollision { id_a: String, id_b: String, pattern: String, method: String },
    #[error("invalid path template: {reason}")]
    InvalidPathTemplate { id: String, reason: String },
    #[error("source '{src}' for route '{id}' requires auth '{required}', got '{got}'")]
    SourceAuthRequirement { id: String, src: String, required: String, got: String },
    #[error("invalid source config for source '{src}' on route '{id}': {reason}")]
    InvalidSourceConfig { id: String, src: String, reason: String },
    #[error("invalid response spec for route '{id}' in mode '{mode}': {reason}")]
    InvalidResponseSpec { id: String, mode: String, reason: String },
    #[error("unknown streaming source '{src}' for route '{id}'")]
    UnknownStreamingSource { id: String, src: String },
    #[error("{0}")]
    Other(String),
}

#[derive(thiserror::Error, Debug)]
pub enum RouteDispatchError {
    #[error("not found")]
    NotFound,
    #[error("bad Last-Event-ID header")]
    BadLastEventId,
    #[error("unauthorized")]
    Unauthorized,
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("internal: {0}")]
    Internal(String),
}

impl axum::response::IntoResponse for RouteDispatchError {
    fn into_response(self) -> axum::response::Response {
        use axum::http::StatusCode;
        let (status, message) = match &self {
            Self::NotFound => (StatusCode::NOT_FOUND, "not found"),
            Self::BadLastEventId => (StatusCode::BAD_REQUEST, "bad Last-Event-ID header"),
            Self::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized"),
            Self::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg.as_str()),
            Self::Internal(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg.as_str()),
        };
        (status, axum::Json(serde_json::json!({ "error": message }))).into_response()
    }
}

impl DispatchError {
    /// Map the typed variant to the HTTP status `/execute` returns.
    /// `api/execute.rs` calls this once per error path; there is no
    /// substring fallback anywhere else.
    pub fn http_status(&self) -> StatusCode {
        match self {
            Self::InvalidRef(..)
            | Self::AliasCycle { .. }
            | Self::AliasChainTooLong { .. }
            | Self::CapabilityRejected { .. }
            | Self::SchemaMisconfigured { .. } => StatusCode::BAD_REQUEST,
            Self::InsufficientCaps { .. }
            | Self::ServiceCapDenied { .. } => StatusCode::FORBIDDEN,
            Self::NotRootExecutable { .. } | Self::StreamingNotImplemented => {
                StatusCode::NOT_IMPLEMENTED
            }
            // State-conflict: push-first, checkout race, etc.
            Self::ProjectSource(_)
            | Self::ProjectSourcePushFirst(_) => StatusCode::CONFLICT,
            // Bad gateway: the daemon reached out to a subsystem
            // (service handler, runtime binary, CAS) and it was
            // missing, unavailable, or returned an error.
            Self::ServiceHandlerMissing { .. }
            | Self::ServiceUnavailable { .. }
            | Self::SubprocessExecutorMissing { .. }
            | Self::SubprocessRunFailed { .. }
            | Self::RuntimeMaterializationFailed { .. }
            | Self::ProjectSourceCheckoutFailed(_) => StatusCode::BAD_GATEWAY,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}
