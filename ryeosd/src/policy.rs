use axum::http::StatusCode;
use axum::Json;
use serde_json::{json, Value};

use crate::auth::Principal;
use crate::state::AppState;

/// Canonical principal ID, always in `fp:<hex>` format.
pub fn request_principal_id(
    request: &axum::http::Request<axum::body::Body>,
    _state: &AppState,
) -> String {
    match request.extensions().get::<Principal>() {
        Some(p) => normalize_fingerprint(&p.fingerprint),
        None => {
            // Auth disabled — use daemon's own identity as principal
            _state.identity.principal_id()
        }
    }
}

/// Extract principal scopes. Returns ["*"] when auth is disabled.
pub fn request_scopes(request: &axum::http::Request<axum::body::Body>) -> Vec<String> {
    match request.extensions().get::<Principal>() {
        Some(p) => p.scopes.clone(),
        None => vec!["*".to_string()],
    }
}

/// Check that scopes include a required scope (or wildcard).
/// Returns Err with 403 FORBIDDEN if insufficient.
pub fn require_scope(scopes: &[String], required: &str) -> Result<(), (StatusCode, Json<Value>)> {
    if scopes.iter().any(|s| s == "*" || s == required) {
        Ok(())
    } else {
        Err((
            StatusCode::FORBIDDEN,
            Json(json!({ "error": format!("insufficient scope: '{}' required", required) })),
        ))
    }
}

/// Normalize a fingerprint to canonical `fp:<hex>` format.
fn normalize_fingerprint(fp: &str) -> String {
    if fp.starts_with("fp:") {
        fp.to_string()
    } else {
        format!("fp:{fp}")
    }
}

/// Log an internal error and return a generic 500 response.
/// Use this for ALL internal errors across all handlers.
pub fn internal_error(err: anyhow::Error) -> (StatusCode, Json<Value>) {
    tracing::error!(error = %err, "internal server error");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": "internal server error" })),
    )
}

// ── Thread ownership ─────────────────────────────────────────────────

/// Check that the caller owns a thread (or has wildcard scope).
/// A principal owns a thread if they created it (requested_by matches).
pub fn check_thread_access(
    caller_principal: &str,
    caller_scopes: &[String],
    thread_requested_by: Option<&str>,
) -> Result<(), (StatusCode, Json<Value>)> {
    if caller_scopes.iter().any(|s| s == "*") {
        return Ok(());
    }
    if caller_scopes.iter().any(|s| s == "threads.admin") {
        return Ok(());
    }
    match thread_requested_by {
        Some(owner) if owner == caller_principal => Ok(()),
        _ => Err((
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "access denied: not thread owner" })),
        )),
    }
}
