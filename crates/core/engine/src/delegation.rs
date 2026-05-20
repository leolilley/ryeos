use std::time::{SystemTime, UNIX_EPOCH};

use crate::contracts::DelegatedPrincipal;
use crate::error::EngineError;

const CLOCK_SKEW_TOLERANCE_SECS: i64 = 30;

fn days_from_civil(y: i32, m: u32, d: u32) -> i64 {
    let (y_adj, m_adj) = if m <= 2 {
        (y as i64 - 1, m as i64 + 9)
    } else {
        (y as i64, m as i64 - 3)
    };
    let era = if y_adj >= 0 {
        y_adj / 400
    } else {
        (y_adj - 399) / 400
    };
    let yoe = y_adj - era * 400;
    let doy = (153 * m_adj + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn parse_rfc3339_to_epoch_secs(s: &str) -> Result<i64, EngineError> {
    let s = s.trim();
    if s.len() < 19 {
        return Err(EngineError::DelegationValidationFailed {
            reason: format!("invalid expires_at timestamp `{}`: too short", s),
        });
    }
    let year: i32 = s[0..4].parse().map_err(|_| EngineError::DelegationValidationFailed {
        reason: format!("invalid expires_at timestamp `{}`: bad year", s),
    })?;
    let month: u32 = s[5..7].parse().map_err(|_| EngineError::DelegationValidationFailed {
        reason: format!("invalid expires_at timestamp `{}`: bad month", s),
    })?;
    let day: u32 = s[8..10].parse().map_err(|_| EngineError::DelegationValidationFailed {
        reason: format!("invalid expires_at timestamp `{}`: bad day", s),
    })?;
    let hour: u32 = s[11..13].parse().map_err(|_| EngineError::DelegationValidationFailed {
        reason: format!("invalid expires_at timestamp `{}`: bad hour", s),
    })?;
    let minute: u32 = s[14..16].parse().map_err(|_| EngineError::DelegationValidationFailed {
        reason: format!("invalid expires_at timestamp `{}`: bad minute", s),
    })?;
    let second: u32 = s[17..19].parse().map_err(|_| EngineError::DelegationValidationFailed {
        reason: format!("invalid expires_at timestamp `{}`: bad second", s),
    })?;

    let days = days_from_civil(year, month, day);
    Ok(days * 86400 + hour as i64 * 3600 + minute as i64 * 60 + second as i64)
}

fn now_epoch_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

pub fn validate_delegation(
    delegation: &DelegatedPrincipal,
    local_site_id: &str,
) -> Result<(), EngineError> {
    if delegation.audience_site_id != local_site_id {
        return Err(EngineError::DelegationValidationFailed {
            reason: format!(
                "audience mismatch: expected `{}`, got `{}`",
                local_site_id, delegation.audience_site_id
            ),
        });
    }

    let expires_epoch = parse_rfc3339_to_epoch_secs(&delegation.expires_at)?;
    let now = now_epoch_secs();
    if expires_epoch < now - CLOCK_SKEW_TOLERANCE_SECS {
        return Err(EngineError::DelegationValidationFailed {
            reason: format!(
                "delegation expired at {} (current time: {}, tolerance: {}s)",
                delegation.expires_at,
                lillux::time::iso8601_now(),
                CLOCK_SKEW_TOLERANCE_SECS
            ),
        });
    }

    Ok(())
}

pub fn is_non_redelegable(delegation: &DelegatedPrincipal) -> bool {
    delegation.non_redelegable
}

#[cfg(test)]
fn iso8601_from_epoch_secs(epoch_secs: i64) -> String {
    let days = epoch_secs / 86400;
    let day_secs = (epoch_secs % 86400) as u64;
    let hours = day_secs / 3600;
    let minutes = (day_secs % 3600) / 60;
    let seconds = day_secs % 60;
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}T{hours:02}:{minutes:02}:{seconds:02}Z")
}

#[cfg(test)]
fn civil_from_days(days: i64) -> (i32, u32, u32) {
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::DelegatedPrincipal;

    fn make_delegation(audience: &str, expires_at: &str, non_redelegable: bool) -> DelegatedPrincipal {
        DelegatedPrincipal {
            protocol_version: "1".to_string(),
            delegation_id: "del-001".to_string(),
            caller_fingerprint: "fp-abc".to_string(),
            origin_site_id: "origin-node".to_string(),
            audience_site_id: audience.to_string(),
            delegated_scopes: vec!["ryeos.execute.*".to_string()],
            budget_lease_id: None,
            request_hash: "hash-xyz".to_string(),
            idempotency_key: "idem-001".to_string(),
            issued_at: lillux::time::iso8601_now(),
            expires_at: expires_at.to_string(),
            non_redelegable,
            origin_signature: "sig-placeholder".to_string(),
        }
    }

    fn future_timestamp(secs_from_now: i64) -> String {
        iso8601_from_epoch_secs(now_epoch_secs() + secs_from_now)
    }

    #[test]
    fn valid_delegation_passes() {
        let expires = future_timestamp(300);
        let d = make_delegation("local-node", &expires, false);
        assert!(validate_delegation(&d, "local-node").is_ok());
    }

    #[test]
    fn wrong_audience_rejected() {
        let expires = future_timestamp(300);
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
        let expires = future_timestamp(-300);
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
        let expires = future_timestamp(-20);
        let d = make_delegation("local-node", &expires, false);
        assert!(validate_delegation(&d, "local-node").is_ok());
    }

    #[test]
    fn beyond_clock_skew_tolerance_rejected() {
        let expires = future_timestamp(-60);
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
        let expires = future_timestamp(300);
        let d = make_delegation("local-node", &expires, true);
        assert!(validate_delegation(&d, "local-node").is_ok());
        assert!(is_non_redelegable(&d));
    }

    #[test]
    fn redelegable_flag_detected() {
        let expires = future_timestamp(300);
        let d = make_delegation("local-node", &expires, false);
        assert!(!is_non_redelegable(&d));
    }
}
