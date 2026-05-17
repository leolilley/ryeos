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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_request_roundtrip() {
        #[derive(serde::Deserialize, Debug)]
        struct Foo {
            x: i32,
        }
        let val = serde_json::json!({ "x": 42 });
        let foo: Foo = parse_request(val).unwrap();
        assert_eq!(foo.x, 42);
    }

    #[test]
    fn parse_request_bad_json_returns_bad_request() {
        #[derive(serde::Deserialize, Debug)]
        struct Foo {
            x: i32,
        }
        let val = serde_json::json!({ "wrong": "field" });
        let err = parse_request::<Foo>(val).unwrap_err();
        match err {
            HandlerError::BadRequest(msg) => {
                assert!(msg.contains("missing field"), "got: {msg}");
            }
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[test]
    fn extract_from_anyhow() {
        let original = HandlerError::NotFound;
        let anyhow_err: anyhow::Error = original.into();
        let extracted = extract_handler_error(&anyhow_err);
        assert!(matches!(extracted, Some(HandlerError::NotFound)));
    }

    #[test]
    fn extract_from_anyhow_plain_returns_none() {
        let anyhow_err: anyhow::Error = anyhow::anyhow!("plain error");
        let extracted = extract_handler_error(&anyhow_err);
        assert!(extracted.is_none());
    }

    #[test]
    fn extract_forbidden() {
        let original = HandlerError::Forbidden("nope".into());
        let anyhow_err: anyhow::Error = original.into();
        let extracted = extract_handler_error(&anyhow_err);
        match extracted {
            Some(HandlerError::Forbidden(msg)) => assert_eq!(msg, "nope"),
            other => panic!("expected Forbidden, got {other:?}"),
        }
    }

    #[test]
    fn extract_through_anyhow_context() {
        // Wrap a HandlerError with anyhow::Context — the source-chain
        // walker should still recover the typed error.
        let original = HandlerError::NotFound;
        let anyhow_err: anyhow::Error = anyhow::Error::new(original)
            .context("looking up schedule xyz");
        let extracted = extract_handler_error(&anyhow_err);
        assert!(
            matches!(extracted, Some(HandlerError::NotFound)),
            "expected NotFound through Context, got {:?}",
            extracted
        );
    }

    #[test]
    fn extract_through_double_context() {
        let original = HandlerError::BadRequest("bad input".into());
        let anyhow_err: anyhow::Error = anyhow::Error::new(original)
            .context("first layer")
            .context("second layer");
        let extracted = extract_handler_error(&anyhow_err);
        match extracted {
            Some(HandlerError::BadRequest(msg)) => assert_eq!(msg, "bad input"),
            other => panic!("expected BadRequest through double Context, got {other:?}"),
        }
    }
}
