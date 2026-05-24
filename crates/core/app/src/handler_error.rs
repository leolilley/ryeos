//! Typed handler errors for service handlers.
//!
//! Handlers return `Result<T, HandlerError>` (or `HandlerResult<T>`)
//! instead of bare `anyhow::Result<T>` when they need to produce
//! specific HTTP status codes (404, 403, 400) instead of the default
//! 500.
//!
//! `service_invocation.rs` downcasts the returned `anyhow::Error` and
//! maps each variant to the corresponding `RouteDispatchError`.

/// Typed error for service handlers.
///
/// Handlers that need ownership-denial or input-validation semantics
/// return this instead of `bail!()`. The `service_invocation` boundary
/// maps each variant to a proper HTTP status.
#[derive(thiserror::Error, Debug, Clone)]
pub enum HandlerError {
    /// The requested resource was not found, or the caller is not
    /// authorised to know it exists. Maps to 404.
    #[error("not found")]
    NotFound,

    /// Authenticated but denied. Maps to 403.
    #[error("forbidden: {0}")]
    Forbidden(String),

    /// Malformed or invalid input. Maps to 400.
    #[error("bad request: {0}")]
    BadRequest(String),

    /// Internal server error. Maps to 500.
    #[error("internal: {0}")]
    Internal(String),

    /// Structured error with a machine-readable JSON payload.
    /// The `code` is a stable string clients can branch on;
    /// `body` is the full response object (always an object).
    /// Maps to the HTTP status encoded in `code` semantics
    /// (typically 400 for validation errors).
    ///
    /// This variant exists so handlers can return richer error
    /// payloads (e.g. per-field validation details) without
    /// flattening everything into a plain string.
    #[error("structured error: {code}")]
    Structured { code: String, body: serde_json::Value },
}

/// Convenience alias for handler return types.
pub type HandlerResult<T> = Result<T, HandlerError>;

/// Strict JSON parse helper. Malformed input produces `BadRequest`
/// instead of a generic 500.
pub fn parse_request<T: serde::de::DeserializeOwned>(
    params: serde_json::Value,
) -> HandlerResult<T> {
    serde_json::from_value(params).map_err(|e| HandlerError::BadRequest(format!("{e}")))
}

impl From<serde_json::Error> for HandlerError {
    fn from(e: serde_json::Error) -> Self {
        HandlerError::BadRequest(format!("{e}"))
    }
}

/// Attempt to extract a `HandlerError` from an `anyhow::Error`.
///
/// Returns `Some(HandlerError)` if the error chain contains a
/// `HandlerError`, `None` otherwise. Walks the error source chain
/// to find the typed error.
pub fn extract_handler_error(err: &anyhow::Error) -> Option<HandlerError> {
    // anyhow::Error can wrap HandlerError directly since HandlerError
    // implements std::error::Error. Try downcasting the root first.
    if let Some(he) = err.downcast_ref::<HandlerError>() {
        return Some(he.clone());
    }
    // Walk the source chain.
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(err.as_ref());
    while let Some(e) = source {
        if let Some(he) = e.downcast_ref::<HandlerError>() {
            return Some(he.clone());
        }
        source = e.source();
    }
    None
}
