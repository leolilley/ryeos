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
    /// State conflict — collides with existing state the caller cannot
    /// override but is entitled to be told about. Maps to 409.
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("internal: {0}")]
    Internal(String),
    /// Structured error with a pre-built JSON body.
    /// The stable code is carried through the route boundary for
    /// dispatchers/observability; the body is emitted as-is and should
    /// include the public error code. The originating typed boundary carries
    /// the exact HTTP status; it is never reconstructed from the message.
    #[error("structured error: {code}")]
    Structured {
        code: String,
        status: u16,
        body: serde_json::Value,
    },
}

impl axum::response::IntoResponse for RouteDispatchError {
    fn into_response(self) -> axum::response::Response {
        use axum::http::StatusCode;
        match self {
            Self::Structured { status, body, .. } => {
                let status =
                    StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                (status, axum::Json(body)).into_response()
            }
            Self::NotFound => (
                StatusCode::NOT_FOUND,
                axum::Json(serde_json::json!({ "error": "not found" })),
            )
                .into_response(),
            Self::BadLastEventId => (
                StatusCode::BAD_REQUEST,
                axum::Json(serde_json::json!({ "error": "bad Last-Event-ID header" })),
            )
                .into_response(),
            Self::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                axum::Json(serde_json::json!({ "error": "unauthorized" })),
            )
                .into_response(),
            Self::Forbidden(msg) => (
                StatusCode::FORBIDDEN,
                axum::Json(serde_json::json!({ "error": msg })),
            )
                .into_response(),
            Self::BadRequest(msg) => (
                StatusCode::BAD_REQUEST,
                axum::Json(serde_json::json!({ "error": msg })),
            )
                .into_response(),
            Self::Conflict(msg) => (
                StatusCode::CONFLICT,
                axum::Json(serde_json::json!({ "error": msg })),
            )
                .into_response(),
            Self::Internal(msg) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(serde_json::json!({ "error": msg })),
            )
                .into_response(),
        }
    }
}

#[cfg(test)]
mod tests {
    use axum::body::to_bytes;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use serde_json::json;

    use super::RouteDispatchError;

    #[tokio::test]
    async fn structured_error_response_emits_body_as_is() {
        let body = json!({
            "error": "contract_violation: `surface:test` (1 error, 0 warnings)",
            "error_code": "contract_violation",
            "details": {
                "errors": [{
                    "path": "launch.mode",
                    "code": "enum_mismatch",
                    "expected": "one of [\"cli_exec\", \"daemon_ui\"]",
                    "found": "\"ladnch_browser\"",
                }],
                "warnings": [],
            },
        });

        let response = RouteDispatchError::Structured {
            code: "contract_violation".into(),
            status: StatusCode::BAD_REQUEST.as_u16(),
            body: body.clone(),
        }
        .into_response();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let actual: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(actual, body);
    }

    #[tokio::test]
    async fn conflict_maps_to_409_with_error_body() {
        let msg = "schedule 'snap-track-feed' in this project is registered by a \
                   different principal; deregister it on the remote or run the sync as \
                   its owner";
        let response = RouteDispatchError::Conflict(msg.to_string()).into_response();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let actual: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(actual, json!({ "error": msg }));
    }
}
