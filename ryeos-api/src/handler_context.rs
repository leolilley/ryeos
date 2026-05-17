//! Typed handler context — the single entry point for caller identity.
//!
//! Injected by `service_invocation.rs` and `executor.rs` as `_ctx` into
//! every principal-aware handler. Handlers never access raw
//! `_caller_fingerprint` or `_caller_scopes` directly.
//!
//! ## Fail-closed semantics
//!
//! When a handler's `required_caps` are non-empty, the caller MUST be
//! authenticated — the invoker rejects unauthenticated requests before
//! the handler is reached. When `required_caps` is empty, `_ctx` may
//! be `HandlerContext::default()` (empty fingerprint, empty scopes).
//!
//! ## Ownership semantics (from `ownership.rs`)
//!
//! - Admin bypass: `["*"]` is the only admin scope in v1.
//! - Owner check: fingerprint matches the resource owner.
//! - Not-found-never-forbidden: `require_owner()` returns
//!   `HandlerError::NotFound` (not 403) to avoid leaking resource
//!   existence to non-owners.

use crate::handler_error::HandlerError;

/// Typed caller context, injected by the service invoker for every
/// principal-aware handler.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct HandlerContext {
    /// Fingerprint of the authenticated caller. Empty when unauthenticated.
    #[serde(default)]
    pub fingerprint: String,
    /// Capability scopes granted to the caller. Empty when unauthenticated.
    #[serde(default)]
    pub scopes: Vec<String>,
}

impl HandlerContext {
    /// Returns true when the caller holds the admin wildcard scope `["*"]`.
    pub fn is_admin(&self) -> bool {
        self.scopes.iter().any(|s| s == "*")
    }

    /// Returns true when a fingerprint was injected (caller is authenticated).
    pub fn is_present(&self) -> bool {
        !self.fingerprint.is_empty()
    }

    /// Returns true when the caller is an admin or matches the resource owner.
    pub fn is_owner_or_admin(&self, owner: Option<&str>) -> bool {
        if self.is_admin() {
            return true;
        }
        match owner {
            Some(fp) => fp == self.fingerprint,
            None => false,
        }
    }

    /// Returns `Ok(())` when the caller is the owner or an admin.
    ///
    /// Returns `HandlerError::NotFound` on miss (never 403) to avoid
    /// leaking resource existence to non-owners.
    pub fn require_owner(&self, owner: Option<&str>) -> Result<(), HandlerError> {
        if self.is_owner_or_admin(owner) {
            Ok(())
        } else {
            Err(HandlerError::NotFound)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(fp: &str, scopes: &[&str]) -> HandlerContext {
        HandlerContext {
            fingerprint: fp.to_string(),
            scopes: scopes.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn admin_bypass() {
        assert!(ctx("fp:a", &["*"]).is_admin());
        assert!(ctx("fp:a", &["execute", "*"]).is_admin());
        assert!(!ctx("fp:a", &["execute"]).is_admin());
    }

    #[test]
    fn owner_passes() {
        assert!(ctx("fp:abc", &["execute"]).is_owner_or_admin(Some("fp:abc")));
    }

    #[test]
    fn admin_passes_regardless_of_owner() {
        assert!(ctx("fp:caller", &["*"]).is_owner_or_admin(Some("fp:other")));
    }

    #[test]
    fn non_owner_fails() {
        assert!(!ctx("fp:caller", &["execute"]).is_owner_or_admin(Some("fp:other")));
    }

    #[test]
    fn none_owner_fails_for_non_admin() {
        assert!(!ctx("fp:caller", &["execute"]).is_owner_or_admin(None));
    }

    #[test]
    fn none_owner_passes_for_admin() {
        assert!(ctx("fp:caller", &["*"]).is_owner_or_admin(None));
    }

    #[test]
    fn require_owner_ok() {
        assert!(ctx("fp:abc", &["execute"]).require_owner(Some("fp:abc")).is_ok());
    }

    #[test]
    fn require_owner_not_found() {
        let err = ctx("fp:caller", &["execute"]).require_owner(Some("fp:other")).unwrap_err();
        assert!(matches!(err, HandlerError::NotFound));
    }

    #[test]
    fn is_present_when_fingerprint_set() {
        assert!(ctx("fp:abc", &[]).is_present());
        assert!(!HandlerContext::default().is_present());
    }

    #[test]
    fn scheduler_non_owner_returns_not_found_not_forbidden() {
        let caller = ctx("fp:attacker", &["ryeos.execute.service.scheduler/register"]);
        let err = caller.require_owner(Some("fp:original_owner")).unwrap_err();
        match err {
            HandlerError::NotFound => {}
            HandlerError::Forbidden(msg) => {
                panic!("ownership denial should be NotFound, got Forbidden({msg})")
            }
            other => panic!("expected NotFound, got {other:?}"),
        }
    }
}
