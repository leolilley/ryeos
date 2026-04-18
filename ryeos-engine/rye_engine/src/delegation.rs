//! Delegated principal validation.
//!
//! Validates `DelegatedPrincipal` envelopes before a remote node accepts
//! forwarded work.  Engine-level checks only — cryptographic signature
//! verification of `origin_signature` is a daemon concern.

use chrono::{DateTime, Utc};

use crate::contracts::DelegatedPrincipal;
use crate::error::EngineError;

/// Maximum allowed clock skew when checking expiration (§9).
const CLOCK_SKEW_TOLERANCE_SECS: i64 = 30;

/// Validate a delegated principal envelope against local policy.
///
/// Checks performed:
/// 1. `audience_site_id` matches `local_site_id`
/// 2. `expires_at` is not in the past (with 30 s clock-skew tolerance)
/// 3. `non_redelegable` is respected — this function does not block on it,
///    but callers that intend to re-forward must check the flag themselves.
///    The validation here ensures the envelope is structurally sound.
pub fn validate_delegation(
    delegation: &DelegatedPrincipal,
    local_site_id: &str,
) -> Result<(), EngineError> {
    // 1. Audience check
    if delegation.audience_site_id != local_site_id {
        return Err(EngineError::DelegationValidationFailed {
            reason: format!(
                "audience mismatch: expected `{}`, got `{}`",
                local_site_id, delegation.audience_site_id
            ),
        });
    }

    // 2. Expiry check (with clock-skew tolerance)
    let expires_at = DateTime::parse_from_rfc3339(&delegation.expires_at).map_err(|e| {
        EngineError::DelegationValidationFailed {
            reason: format!("invalid expires_at timestamp `{}`: {}", delegation.expires_at, e),
        }
    })?;

    let now = Utc::now();
    let skew = chrono::Duration::seconds(CLOCK_SKEW_TOLERANCE_SECS);
    if expires_at < now - skew {
        return Err(EngineError::DelegationValidationFailed {
            reason: format!(
                "delegation expired at {} (current time: {}, tolerance: {}s)",
                delegation.expires_at,
                now.to_rfc3339(),
                CLOCK_SKEW_TOLERANCE_SECS
            ),
        });
    }

    Ok(())
}

/// Returns `true` if the delegation is non-redelegable.
///
/// Callers that intend to re-forward must check this before proceeding.
pub fn is_non_redelegable(delegation: &DelegatedPrincipal) -> bool {
    delegation.non_redelegable
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::DelegatedPrincipal;
    use chrono::{Duration, Utc};

    fn make_delegation(audience: &str, expires_at: &str, non_redelegable: bool) -> DelegatedPrincipal {
        DelegatedPrincipal {
            protocol_version: "1".to_string(),
            delegation_id: "del-001".to_string(),
            caller_fingerprint: "fp-abc".to_string(),
            origin_site_id: "origin-node".to_string(),
            audience_site_id: audience.to_string(),
            delegated_scopes: vec!["rye.execute.*".to_string()],
            budget_lease_id: None,
            request_hash: "hash-xyz".to_string(),
            idempotency_key: "idem-001".to_string(),
            issued_at: Utc::now().to_rfc3339(),
            expires_at: expires_at.to_string(),
            non_redelegable,
            origin_signature: "sig-placeholder".to_string(),
        }
    }

    #[test]
    fn valid_delegation_passes() {
        let expires = (Utc::now() + Duration::minutes(5)).to_rfc3339();
        let d = make_delegation("local-node", &expires, false);
        assert!(validate_delegation(&d, "local-node").is_ok());
    }

    #[test]
    fn wrong_audience_rejected() {
        let expires = (Utc::now() + Duration::minutes(5)).to_rfc3339();
        let d = make_delegation("other-node", &expires, false);
        let err = validate_delegation(&d, "local-node").unwrap_err();
        match &err {
            EngineError::DelegationValidationFailed { reason } => {
                assert!(reason.contains("audience mismatch"), "unexpected reason: {reason}");
                assert!(reason.contains("local-node"));
                assert!(reason.contains("other-node"));
            }
            other => panic!("expected DelegationValidationFailed, got: {other:?}"),
        }
    }

    #[test]
    fn expired_delegation_rejected() {
        let expires = (Utc::now() - Duration::minutes(5)).to_rfc3339();
        let d = make_delegation("local-node", &expires, false);
        let err = validate_delegation(&d, "local-node").unwrap_err();
        match &err {
            EngineError::DelegationValidationFailed { reason } => {
                assert!(reason.contains("expired"), "unexpected reason: {reason}");
            }
            other => panic!("expected DelegationValidationFailed, got: {other:?}"),
        }
    }

    #[test]
    fn within_clock_skew_tolerance_passes() {
        // Expired 20 seconds ago — within the 30s tolerance window
        let expires = (Utc::now() - Duration::seconds(20)).to_rfc3339();
        let d = make_delegation("local-node", &expires, false);
        assert!(validate_delegation(&d, "local-node").is_ok());
    }

    #[test]
    fn beyond_clock_skew_tolerance_rejected() {
        // Expired 60 seconds ago — well beyond the 30s tolerance
        let expires = (Utc::now() - Duration::seconds(60)).to_rfc3339();
        let d = make_delegation("local-node", &expires, false);
        assert!(validate_delegation(&d, "local-node").is_err());
    }

    #[test]
    fn invalid_timestamp_rejected() {
        let d = make_delegation("local-node", "not-a-timestamp", false);
        let err = validate_delegation(&d, "local-node").unwrap_err();
        match &err {
            EngineError::DelegationValidationFailed { reason } => {
                assert!(reason.contains("invalid expires_at"), "unexpected reason: {reason}");
            }
            other => panic!("expected DelegationValidationFailed, got: {other:?}"),
        }
    }

    #[test]
    fn non_redelegable_flag_detected() {
        let expires = (Utc::now() + Duration::minutes(5)).to_rfc3339();
        let d = make_delegation("local-node", &expires, true);
        // Validation itself passes — the flag is a policy concern for callers
        assert!(validate_delegation(&d, "local-node").is_ok());
        assert!(is_non_redelegable(&d));
    }

    #[test]
    fn redelegable_flag_detected() {
        let expires = (Utc::now() + Duration::minutes(5)).to_rfc3339();
        let d = make_delegation("local-node", &expires, false);
        assert!(!is_non_redelegable(&d));
    }
}
