//! Wire-shape struct shared by all structured daemon error surfaces:
//! `/execute` JSON body, `/execute/stream` SSE `stream_error` event,
//! and `thread_failed.payload.error`.
//!
//! Keeping a single struct guarantees the three surfaces stay in sync.
//! Adding a new field is a one-line change here, not a three-site
//! search-and-replace.
//!
//! Construction is via `From<&DispatchError>` for the cases that map
//! cleanly. For paths that don't have a `DispatchError` in hand
//! (resume preflight in `runner.rs` only sees `MaterializationError`),
//! call `StructuredErrorPayload::required_secret_missing(...)` or
//! whichever generic constructor matches.

use serde::Serialize;

/// Stable wire shape for structured daemon errors. Every field except
/// `code` and `error` is optional so unrelated error variants can
/// serialize the same struct without writing nulls.
#[derive(Debug, Clone, Serialize)]
pub struct StructuredErrorPayload {
    /// Stable machine-readable error code (e.g. `"required_secret_missing"`).
    pub code: String,
    /// Human-readable error message. Always present.
    pub error: String,
    /// Required-secret-missing fields.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env_var: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remediation: Option<String>,
    /// Cap-denial field (existing `MissingCap` surface).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub required_cap: Option<String>,
}

impl StructuredErrorPayload {
    /// Build a generic envelope with just `code` + `error`. Use for
    /// `DispatchError` variants that have no extra structured fields.
    pub fn generic(code: impl Into<String>, error: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            error: error.into(),
            env_var: None,
            source_kind: None,
            source_name: None,
            remediation: None,
            required_cap: None,
        }
    }

    /// Build the `required_secret_missing` envelope. Used by both
    /// the dispatch boundary (`From<&DispatchError>`) and the resume
    /// preflight path in `runner.rs` (which only has the launch-layer
    /// `MaterializationError` in hand).
    pub fn required_secret_missing(
        error_msg: impl Into<String>,
        env_var: impl Into<String>,
        source_kind: impl Into<String>,
        source_name: impl Into<String>,
        remediation: impl Into<String>,
    ) -> Self {
        Self {
            code: "required_secret_missing".to_string(),
            error: error_msg.into(),
            env_var: Some(env_var.into()),
            source_kind: Some(source_kind.into()),
            source_name: Some(source_name.into()),
            remediation: Some(remediation.into()),
            required_cap: None,
        }
    }

    /// Build the `missing_cap` envelope (existing `/execute` surface).
    pub fn missing_cap(error_msg: impl Into<String>, required: impl Into<String>) -> Self {
        Self {
            code: "missing_cap".to_string(),
            error: error_msg.into(),
            env_var: None,
            source_kind: None,
            source_name: None,
            remediation: None,
            required_cap: Some(required.into()),
        }
    }

    /// Convert to `serde_json::Value` for surfaces that need a `Value`
    /// (the SSE helper, terminal event payload).
    pub fn to_value(&self) -> serde_json::Value {
        serde_json::to_value(self).expect("StructuredErrorPayload always serializes")
    }
}

impl From<&crate::dispatch_error::DispatchError> for StructuredErrorPayload {
    fn from(e: &crate::dispatch_error::DispatchError) -> Self {
        use crate::dispatch_error::DispatchError;
        match e {
            DispatchError::RequiredSecretMissing {
                env_var,
                source_kind,
                source_name,
                remediation,
                ..
            } => Self::required_secret_missing(
                e.to_string(),
                env_var,
                source_kind,
                source_name,
                remediation,
            ),
            DispatchError::MissingCap { required } => {
                Self::missing_cap(e.to_string(), required)
            }
            _ => Self::generic(e.code(), e.to_string()),
        }
    }
}
