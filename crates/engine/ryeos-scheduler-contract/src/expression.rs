//! Pure expression interpretation; timer computation remains scheduler-owned.

use anyhow::{Context, Result, bail};
use std::str::FromStr;

pub fn parse_iso_ts(expression: &str) -> Option<i64> {
    // Try chrono's ISO 8601 parsing
    chrono::DateTime::parse_from_rfc3339(expression)
        .ok()
        .map(|dt| dt.timestamp_millis())
        .or_else(|| {
            // Try without timezone (assume UTC)
            let with_z = format!("{}Z", expression);
            chrono::DateTime::parse_from_rfc3339(&with_z)
                .ok()
                .map(|dt| dt.timestamp_millis())
        })
}

/// Validate a schedule expression parses correctly for the given type.
pub fn validate_expression(schedule_type: &str, expression: &str) -> Result<()> {
    match schedule_type {
        "cron" => {
            cron::Schedule::from_str(expression)
                .with_context(|| format!("invalid cron expression: {}", expression))?;
            Ok(())
        }
        "interval" => {
            let secs: i64 = expression
                .parse()
                .with_context(|| format!("interval must be integer seconds: {}", expression))?;
            if secs <= 0 {
                bail!("interval must be positive, got: {}", secs);
            }
            Ok(())
        }
        "at" => {
            if parse_iso_ts(expression).is_none() {
                bail!(
                    "invalid at expression (must be ISO 8601 timestamp): {}",
                    expression
                );
            }
            Ok(())
        }
        other => bail!("unknown schedule_type: {}", other),
    }
}

/// Validate a timezone string.
pub fn validate_timezone(tz: &str) -> Result<()> {
    tz.parse::<chrono_tz::Tz>()
        .with_context(|| format!("invalid IANA timezone: {}", tz))?;
    Ok(())
}

/// Validate the canonical schedule identity shared with durable fire records.
pub fn validate_schedule_id(id: &str) -> Result<()> {
    ryeos_state::gc::retention::validate_schedule_id(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    // ── validate_expression ──────────────────────────────────────

    #[test]
    fn validate_cron_every_minute() {
        assert!(validate_expression("cron", "* * * * * *").is_ok());
    }

    #[test]
    fn validate_cron_every_hour() {
        assert!(validate_expression("cron", "0 * * * * *").is_ok());
    }

    #[test]
    fn validate_cron_invalid() {
        assert!(validate_expression("cron", "not-a-cron").is_err());
    }

    #[test]
    fn validate_interval_positive() {
        assert!(validate_expression("interval", "60").is_ok());
    }

    #[test]
    fn validate_interval_zero() {
        assert!(validate_expression("interval", "0").is_err());
    }

    #[test]
    fn validate_interval_negative() {
        assert!(validate_expression("interval", "-1").is_err());
    }

    #[test]
    fn validate_interval_non_numeric() {
        assert!(validate_expression("interval", "abc").is_err());
    }

    #[test]
    fn validate_at_valid_rfc3339() {
        assert!(validate_expression("at", "2026-01-01T00:00:00Z").is_ok());
    }

    #[test]
    fn validate_at_invalid() {
        assert!(validate_expression("at", "not-a-date").is_err());
    }

    #[test]
    fn validate_unknown_type() {
        assert!(validate_expression("unknown", "anything").is_err());
    }

    // ── validate_timezone ────────────────────────────────────────

    #[test]
    fn valid_timezone_utc() {
        assert!(validate_timezone("UTC").is_ok());
    }

    #[test]
    fn valid_timezone_america_new_york() {
        assert!(validate_timezone("America/New_York").is_ok());
    }

    #[test]
    fn invalid_timezone() {
        assert!(validate_timezone("Invalid/Zone").is_err());
    }

    // ── validate_schedule_id ─────────────────────────────────────

    #[test]
    fn valid_schedule_id() {
        assert!(validate_schedule_id("my-schedule").is_ok());
        assert!(validate_schedule_id("schedule_123").is_ok());
    }

    #[test]
    fn invalid_schedule_id_empty() {
        assert!(validate_schedule_id("").is_err());
    }

    #[test]
    fn invalid_schedule_id_slash() {
        assert!(validate_schedule_id("bad/id").is_err());
    }

    #[test]
    fn invalid_schedule_id_dot() {
        assert!(validate_schedule_id(".").is_err());
        assert!(validate_schedule_id("..").is_err());
    }

    #[test]
    fn invalid_schedule_id_whitespace() {
        assert!(validate_schedule_id("has space").is_err());
    }
}
