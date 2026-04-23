//! Scope enforcement — checks whether a principal has permission to execute.
//!
//! The engine checks scopes during `build_plan`. Scopes are plain strings;
//! the engine never interprets their contents beyond two checks:
//!
//! - `"*"` is the wildcard scope (grants everything)
//! - `"execute"` is the minimum scope required to run items
//!
//! Fail-closed: missing scopes = denied.

use crate::contracts::EffectivePrincipal;
use crate::error::EngineError;

/// The scope string required for item execution.
const EXECUTE_SCOPE: &str = "execute";

/// The wildcard scope that grants all permissions.
const WILDCARD_SCOPE: &str = "*";

/// Check that the principal has permission to execute an item.
///
/// For `Local` principals, checks `scopes`.
/// For `Delegated` principals, checks `delegated_scopes`.
///
/// Returns `Ok(())` if the principal has either `"*"` or `"execute"`.
/// Returns `EngineError::InsufficientScope` otherwise.
pub fn check_execution_scope(principal: &EffectivePrincipal) -> Result<(), EngineError> {
    let scopes = match principal {
        EffectivePrincipal::Local(p) => &p.scopes,
        EffectivePrincipal::Delegated(d) => &d.delegated_scopes,
    };

    let has_permission = scopes
        .iter()
        .any(|s| s == WILDCARD_SCOPE || s == EXECUTE_SCOPE);

    if has_permission {
        Ok(())
    } else {
        Err(EngineError::InsufficientScope {
            required: EXECUTE_SCOPE.to_owned(),
            available: scopes.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::{DelegatedPrincipal, Principal};

    fn local_principal(scopes: Vec<&str>) -> EffectivePrincipal {
        EffectivePrincipal::Local(Principal {
            fingerprint: "fp:test".into(),
            scopes: scopes.into_iter().map(String::from).collect(),
        })
    }

    fn delegated_principal(scopes: Vec<&str>) -> EffectivePrincipal {
        EffectivePrincipal::Delegated(DelegatedPrincipal {
            protocol_version: "1".into(),
            delegation_id: "del:1".into(),
            caller_fingerprint: "fp:caller".into(),
            origin_site_id: "site:origin".into(),
            audience_site_id: "site:audience".into(),
            delegated_scopes: scopes.into_iter().map(String::from).collect(),
            budget_lease_id: None,
            request_hash: "hash".into(),
            idempotency_key: "key".into(),
            issued_at: "2026-01-01T00:00:00Z".into(),
            expires_at: "2026-12-31T23:59:59Z".into(),
            non_redelegable: false,
            origin_signature: "sig".into(),
        })
    }

    #[test]
    fn local_with_execute_scope() {
        assert!(check_execution_scope(&local_principal(vec!["execute"])).is_ok());
    }

    #[test]
    fn local_with_wildcard_scope() {
        assert!(check_execution_scope(&local_principal(vec!["*"])).is_ok());
    }

    #[test]
    fn local_with_execute_among_others() {
        assert!(
            check_execution_scope(&local_principal(vec!["threads.read", "execute", "registry.read"]))
                .is_ok()
        );
    }

    #[test]
    fn local_with_no_scopes_denied() {
        let err = check_execution_scope(&local_principal(vec![])).unwrap_err();
        assert!(
            matches!(err, EngineError::InsufficientScope { ref required, ref available }
                if required == "execute" && available.is_empty()),
            "expected InsufficientScope, got: {err:?}"
        );
    }

    #[test]
    fn local_with_wrong_scopes_denied() {
        let err =
            check_execution_scope(&local_principal(vec!["threads.read", "registry.read"]))
                .unwrap_err();
        assert!(
            matches!(err, EngineError::InsufficientScope { .. }),
            "expected InsufficientScope, got: {err:?}"
        );
    }

    #[test]
    fn delegated_with_execute_scope() {
        assert!(check_execution_scope(&delegated_principal(vec!["execute"])).is_ok());
    }

    #[test]
    fn delegated_with_wildcard_scope() {
        assert!(check_execution_scope(&delegated_principal(vec!["*"])).is_ok());
    }

    #[test]
    fn delegated_with_no_scopes_denied() {
        let err = check_execution_scope(&delegated_principal(vec![])).unwrap_err();
        assert!(
            matches!(err, EngineError::InsufficientScope { .. }),
            "expected InsufficientScope, got: {err:?}"
        );
    }

    #[test]
    fn delegated_with_wrong_scopes_denied() {
        let err =
            check_execution_scope(&delegated_principal(vec!["registry.read"])).unwrap_err();
        assert!(
            matches!(err, EngineError::InsufficientScope { .. }),
            "expected InsufficientScope, got: {err:?}"
        );
    }
}
