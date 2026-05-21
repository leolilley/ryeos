//! Route-config and route-dispatch errors.
//!
//! Extracted from the unified `dispatch_error` module so the daemon
//! retains its route-specific error types while the executor crate
//! owns `DispatchError`. Move target for Phase D: relocate alongside
//! the route compilation surface.

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
    PathCollision {
        id_a: String,
        id_b: String,
        pattern: String,
        method: String,
    },
    #[error("invalid path template: {reason}")]
    InvalidPathTemplate { id: String, reason: String },
    #[error("source '{src}' for route '{id}' requires auth '{required}', got '{got}'")]
    SourceAuthRequirement {
        id: String,
        src: String,
        required: String,
        got: String,
    },
    #[error("invalid source config for source '{src}' on route '{id}': {reason}")]
    InvalidSourceConfig {
        id: String,
        src: String,
        reason: String,
    },
    #[error("invalid response spec for route '{id}' in mode '{mode}': {reason}")]
    InvalidResponseSpec {
        id: String,
        mode: String,
        reason: String,
    },
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
    /// Authenticated but denied — the caller's scopes don't include
    /// a required capability. Maps to 403 (not 401, since the identity
    /// is verified).
    #[error("forbidden: {0}")]
    Forbidden(String),
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
            Self::Forbidden(msg) => (StatusCode::FORBIDDEN, msg.as_str()),
            Self::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg.as_str()),
            Self::Internal(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg.as_str()),
        };
        (status, axum::Json(serde_json::json!({ "error": message }))).into_response()
    }
}
