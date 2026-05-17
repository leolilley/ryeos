//! Shared ownership-check helpers for thread and scheduler handlers.
//!
//! Centralises the "is the caller the owner or an admin?" check so
//! every handler uses the same logic. Returns `HandlerError::NotFound`
//! (never 403) to avoid leaking resource existence to non-owners.

use crate::handler_error::HandlerError;

/// `["*"]` is the only admin bypass in v1.
pub fn is_admin(scopes: &[String]) -> bool {
    scopes.iter().any(|s| s == "*")
}

/// Returns `true` when the caller is an admin or is the owner.
pub fn is_owner_or_admin(
    owner_fingerprint: Option<&str>,
    caller_fingerprint: &str,
    caller_scopes: &[String],
) -> bool {
    if is_admin(caller_scopes) {
        return true;
    }
    match owner_fingerprint {
        Some(fp) => fp == caller_fingerprint,
        None => false,
    }
}

/// Returns `Ok(())` when the caller is the owner or an admin.
///
/// Returns `HandlerError::NotFound` on miss — never 403 — to avoid
/// leaking existence to non-owners.
pub fn require_owner_or_admin(
    owner_fingerprint: Option<&str>,
    caller_fingerprint: &str,
    caller_scopes: &[String],
) -> Result<(), HandlerError> {
    if is_owner_or_admin(owner_fingerprint, caller_fingerprint, caller_scopes) {
        Ok(())
    } else {
        Err(HandlerError::NotFound)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admin_bypass() {
        assert!(is_admin(&["*".to_string()]));
        assert!(is_admin(&["execute".to_string(), "*".to_string()]));
        assert!(!is_admin(&["execute".to_string()]));
    }

    #[test]
    fn owner_passes() {
        assert!(is_owner_or_admin(
            Some("fp:abc"),
            "fp:abc",
            &["execute".to_string()],
        ));
    }

    #[test]
    fn admin_passes_regardless_of_owner() {
        assert!(is_owner_or_admin(
            Some("fp:other"),
            "fp:caller",
            &["*".to_string()],
        ));
    }

    #[test]
    fn non_owner_fails() {
        assert!(!is_owner_or_admin(
            Some("fp:other"),
            "fp:caller",
            &["execute".to_string()],
        ));
    }

    #[test]
    fn none_owner_fails_for_non_admin() {
        assert!(!is_owner_or_admin(
            None,
            "fp:caller",
            &["execute".to_string()],
        ));
    }

    #[test]
    fn none_owner_passes_for_admin() {
        assert!(is_owner_or_admin(None, "fp:caller", &["*".to_string()]));
    }

    #[test]
    fn require_owner_or_admin_ok() {
        assert!(require_owner_or_admin(
            Some("fp:abc"),
            "fp:abc",
            &["execute".to_string()],
        )
        .is_ok());
    }

    #[test]
    fn require_owner_or_admin_not_found() {
        let err = require_owner_or_admin(
            Some("fp:other"),
            "fp:caller",
            &["execute".to_string()],
        )
        .unwrap_err();
        assert!(matches!(err, HandlerError::NotFound));
    }

    #[test]
    fn scheduler_non_owner_returns_not_found_not_forbidden() {
        // Simulate the exact path scheduler_register takes: ownership
        // check fails → HandlerError::NotFound (never 403).
        let owner = "fp:original_owner";
        let caller = "fp:attacker";
        let caller_scopes = vec!["ryeos.execute.service.scheduler/register".to_string()];

        let err = require_owner_or_admin(Some(owner), caller, &caller_scopes).unwrap_err();
        match err {
            HandlerError::NotFound => {} // correct — does not leak existence
            HandlerError::Forbidden(msg) => panic!(
                "ownership denial should be NotFound, got Forbidden({msg}) — \
                 this would leak resource existence"
            ),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }
}
