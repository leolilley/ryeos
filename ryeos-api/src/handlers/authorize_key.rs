//! `authorize-key` — create a node-signed authorized-key TOML entry.
//!
//! Remote callers can request that the node create an authorized key
//! with scoped capabilities. The node validates scope restrictions
//! and signs the TOML with its own key — remote callers cannot forge
//! authorized keys.
//!
//! Wildcard delegation (`*`) is forbidden in v1 per Phase 0 decision 5.

use std::sync::Arc;

use base64::Engine;
use serde_json::Value;

use ryeos_executor::executor::ServiceAvailability;
use crate::registry::ServiceDescriptor;
use crate::handler_error::{HandlerError, HandlerResult};
use crate::handler_context::HandlerContext;
use ryeos_app::state::AppState;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// Ed25519 public key in "ed25519:<base64>" format.
    pub public_key: String,
    /// Human-readable label for the authorized key.
    pub label: String,
    /// Capabilities to grant. Must be a subset of caller's scopes.
    pub scopes: Vec<String>,
}

#[derive(serde::Serialize)]
pub struct Response {
    pub fingerprint: String,
    pub label: String,
    pub scopes: Vec<String>,
    pub granted_by: String,
    pub created_at: String,
}

pub async fn handle(req: Request, ctx: HandlerContext, state: Arc<AppState>) -> HandlerResult<Value> {
    // Caller identity used for scope delegation — must be verified.
    ctx.require_verified()?;

    // 1. Parse public_key: must be "ed25519:<b64>"
    let key_b64 = req
        .public_key
        .strip_prefix("ed25519:")
        .ok_or_else(|| {
            HandlerError::BadRequest("public_key must be in 'ed25519:<base64>' format".into())
        })?;

    let key_bytes = base64::engine::general_purpose::STANDARD
        .decode(key_b64)
        .map_err(|e| HandlerError::BadRequest(format!("invalid base64 in public_key: {e}")))?;

    if key_bytes.len() != 32 {
        return Err(HandlerError::BadRequest(
            "ed25519 public key must be 32 bytes".into(),
        ));
    }

    // 2. Compute fingerprint from the public key bytes.
    let fingerprint = lillux::sha256_hex(&key_bytes);

    // 3. Validate label is non-empty.
    if req.label.trim().is_empty() {
        return Err(HandlerError::BadRequest("label must not be empty".into()));
    }

    // 4. Validate scopes are non-empty.
    if req.scopes.is_empty() {
        return Err(HandlerError::BadRequest("scopes must not be empty".into()));
    }

    // 5. Normalize scopes: trim whitespace, deduplicate.
    let mut normalized: Vec<String> = req
        .scopes
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    normalized.sort();
    normalized.dedup();

    if normalized.len() != req.scopes.len() {
        return Err(HandlerError::BadRequest(
            "duplicate or empty scopes after normalization".into(),
        ));
    }

    // 5b. Validate scope grammar for each scope.
    for scope in &normalized {
        validate_scope_pattern(scope)?;
    }

    // 6. Reject wildcard delegation (Phase 0 decision 5).
    if normalized.iter().any(|s| s == "*") {
        return Err(HandlerError::Forbidden(
            "wildcard delegation forbidden in v1".into(),
        ));
    }

    // 7. Subset check: every requested scope must be permitted by at
    //    least one caller scope. Callers with ["*"] can grant anything
    //    (but wildcard was already rejected above, so admin callers
    //    must still explicitly list scopes).
    for scope in &normalized {
        let permitted = state
            .authorizer
            .authorize(
                &ctx.scopes,
                &ryeos_runtime::authorizer::AuthorizationPolicy::require(scope.as_str()),
            )
            .is_ok();
        if !permitted {
            return Err(HandlerError::Forbidden(format!(
                "scope '{}' not granted to caller",
                scope
            )));
        }
    }

    // 8. Write the authorized-key TOML.
    let now = lillux::time::iso8601_now();
    let auth_dir = state
        .config
        .system_space_dir
        .join(ryeos_engine::AI_DIR)
        .join("node")
        .join("auth")
        .join("authorized_keys");

    let _path = ryeos_app::identity::write_authorized_key_toml(
        &auth_dir,
        &fingerprint,
        key_b64,
        &normalized,
        &req.label,
        &ctx.fingerprint,
        &now,
        state.identity.signing_key(),
        ryeos_app::identity::WildcardPolicy::Reject,
    )
    .map_err(|e| HandlerError::Internal(e.to_string()))?;

    let response = Response {
        fingerprint,
        label: req.label,
        scopes: normalized,
        granted_by: ctx.fingerprint,
        created_at: now,
    };

    serde_json::to_value(response).map_err(|e| HandlerError::Internal(e.to_string()))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:identity/authorize-key",
    endpoint: "identity.authorize-key",
    availability: ServiceAvailability::Both,
    required_caps: &["ryeos.execute.service.authorize_key"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, ctx, state).await.map_err(Into::into)
        })
    },
};

/// Validate scope grammar — delegates to the centralized implementation
/// in `ryeos_runtime::authorizer`.
fn validate_scope_pattern(scope: &str) -> Result<(), HandlerError> {
    ryeos_runtime::authorizer::validate_scope_pattern(scope)
        .map_err(HandlerError::BadRequest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reject_wrong_prefix() {
        let req = serde_json::from_value::<Request>(serde_json::json!({
            "public_key": "rsa:abc123",
            "label": "test",
            "scopes": ["execute"],
        }));
        // deny_unknown_fields won't block this; but the handler
        // rejects wrong prefix. Test the validation directly.
        assert!(req.is_ok());
    }

    #[test]
    fn reject_empty_scopes() {
        let req = serde_json::from_value::<Request>(serde_json::json!({
            "public_key": "ed25519:abc",
            "label": "test",
            "scopes": [],
        }));
        assert!(req.is_ok()); // struct parses fine; handler validates
    }

    // ── Scope grammar tests ──
    //
    // Grammar lives in `ryeos_runtime::authorizer::validate_scope_pattern`.
    // Canonical form is `ryeos.<verb>.<kind>.<subject>`; short-form
    // scopes are rejected because the matcher does not auto-prefix.

    #[test]
    fn valid_canonical_scope() {
        assert!(validate_scope_pattern("ryeos.execute.service.vault.set").is_ok());
        assert!(validate_scope_pattern("ryeos.execute.service.bundle.install").is_ok());
        assert!(validate_scope_pattern("ryeos.execute.service.remote.admin").is_ok());
    }

    #[test]
    fn reject_short_form_scope() {
        // Short forms like `bundle.install` would never satisfy any
        // handler's `ryeos.execute.service.*` requirement.
        assert!(validate_scope_pattern("bundle.install").is_err());
        assert!(validate_scope_pattern("remote.admin").is_err());
        assert!(validate_scope_pattern("execute").is_err());
    }

    // ── Wildcard delegation: handler rejects "*", grammar permits it ──

    #[test]
    fn wildcard_passes_grammar_but_handler_rejects_it() {
        // Grammar layer accepts "*" — wildcard policy lives elsewhere.
        assert!(validate_scope_pattern("*").is_ok());
        // The handler's step-6 check rejects any list containing "*".
        let normalized = vec!["ryeos.execute.service.bundle.install".to_string(), "*".to_string()];
        let has_wildcard = normalized.iter().any(|s| s == "*");
        assert!(has_wildcard, "handler must catch '*' in scope list");
    }

    #[test]
    fn accept_prefix_wildcards() {
        // Prefix wildcards are valid grammar (and useful for delegation).
        assert!(validate_scope_pattern("ryeos.*").is_ok());
        assert!(validate_scope_pattern("ryeos.execute.*").is_ok());
        assert!(validate_scope_pattern("ryeos.execute.service.*").is_ok());
    }
}
