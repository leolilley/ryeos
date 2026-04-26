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
    #[error("internal: {0}")]
    Internal(#[from] anyhow::Error),
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
            Self::InsufficientCaps { .. } => StatusCode::FORBIDDEN,
            Self::NotRootExecutable { .. } | Self::StreamingNotImplemented => {
                StatusCode::NOT_IMPLEMENTED
            }
            // V5.3 keeps the V5.2 contract: any project-source failure
            // (push-first, missing CAS HEAD, ...) maps to 409 — this
            // is a state-conflict, not a malformed request.
            Self::ProjectSource(_) => StatusCode::CONFLICT,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}
