//! Cron expression parsing and next-fire computation.
//!
//! Uses the `cron` crate (v0.16.0) for cron expressions.
//! Interval and at types are handled directly.

use anyhow::{bail, Context, Result};
use std::str::FromStr;

/// Compute the next fire time (millis since epoch) after `after_ms`.
///
/// Returns `None` if:
/// - `at` schedule: timestamp is in the past
/// - Any schedule: expression cannot produce a future fire time
pub fn compute_next_fire(
    schedule_type: &str,
    expression: &str,
    timezone: &str,
    after_ms: i64,
    last_fire_at: Option<i64>,
) -> Result<Option<i64>> {
    match schedule_type {
        "cron" => next_cron_fire(expression, timezone, after_ms),
        "interval" => next_interval_fire(expression, after_ms, last_fire_at),
        "at" => next_at_fire(expression, after_ms),
        other => bail!("unknown schedule_type: {}", other),
    }
}

/// Compute the `scheduled_at` for the current fire.
/// For cron/interval: finds the exact boundary.
/// For at: returns the given timestamp.
///
/// `registration_time` is used as the fallback start bound for cron schedules
/// that have never fired, preventing an epoch-to-now walk that would iterate
/// millions of times for fine-grained cron expressions.
pub fn compute_scheduled_at(
    schedule_type: &str,
    expression: &str,
    timezone: &str,
    now_ms: i64,
    last_fire_at: Option<i64>,
    registration_time: Option<i64>,
) -> Option<i64> {
    match schedule_type {
        "cron" => {
            // Find the most recent fire boundary at or before now.
            // Use registration_time (not 0) as fallback to avoid walking from epoch.
            let after = last_fire_at.or(registration_time).unwrap_or(0);
            find_most_recent_cron(expression, timezone, after, now_ms)
        }
        "interval" => {
            let interval_secs = expression.parse::<i64>().ok()?;
            if interval_secs <= 0 {
                return None;
            }
            let interval_ms = interval_secs * 1000;
            last_fire_at.map(|last| last + interval_ms)
        }
        "at" => {
            parse_iso_ts(expression)
        }
        _ => None,
    }
}

// ── Cron ────────────────────────────────────────────────────────────

fn next_cron_fire(expression: &str, timezone: &str, after_ms: i64) -> Result<Option<i64>> {
    let schedule = cron::Schedule::from_str(expression)
        .with_context(|| format!("invalid cron expression: {}", expression))?;

    let after = chrono::DateTime::from_timestamp_millis(after_ms)
        .unwrap_or_else(chrono::Utc::now);

    let tz: chrono_tz::Tz = timezone.parse()
        .with_context(|| format!("invalid IANA timezone: {}", timezone))?;

    let after_with_tz = after.with_timezone(&tz);

    match schedule.after(&after_with_tz).next() {
        Some(dt) => Ok(Some(dt.timestamp_millis())),
        None => Ok(None),
    }
}

/// Walk cron fire times between `start_ms` and `end_ms` and return
/// the most recent one that is <= `end_ms`.
///
/// Caps iteration at 10 000 steps to prevent runaway loops on pathological
/// cron expressions (e.g. every second over a multi-year span).
fn find_most_recent_cron(expression: &str, timezone: &str, start_ms: i64, end_ms: i64) -> Option<i64> {
    let schedule = cron::Schedule::from_str(expression).ok()?;
    let tz: chrono_tz::Tz = timezone.parse().ok()?;
    let start = chrono::DateTime::from_timestamp_millis(start_ms)?;
    let start_with_tz = start.with_timezone(&tz);

    let mut most_recent: Option<i64> = None;
    const MAX_ITERATIONS: usize = 10_000;
    for (i, dt) in schedule.after(&start_with_tz).enumerate() {
        if i >= MAX_ITERATIONS {
            tracing::warn!(
                expression = %expression,
                "find_most_recent_cron hit iteration cap ({MAX_ITERATIONS}) — using last found boundary"
            );
            break;
        }
        let ms = dt.timestamp_millis();
        if ms > end_ms {
            break;
        }
        most_recent = Some(ms);
    }
    most_recent
}

// ── Interval ────────────────────────────────────────────────────────

fn next_interval_fire(expression: &str, now_ms: i64, last_fire_at: Option<i64>) -> Result<Option<i64>> {
    let interval_secs: i64 = expression.parse()
        .with_context(|| format!("invalid interval expression (must be integer seconds): {}", expression))?;
    if interval_secs <= 0 {
        bail!("interval must be positive, got: {}", interval_secs);
    }
    let interval_ms = interval_secs * 1000;

    Ok(match last_fire_at {
        Some(last) => {
            let next = last + interval_ms;
            if next < now_ms {
                // Jump ahead to now-aligned boundary
                let elapsed = now_ms - last;
                let intervals_elapsed = elapsed / interval_ms;
                Some(last + (intervals_elapsed + 1) * interval_ms)
            } else {
                Some(next)
            }
        }
        None => Some(now_ms + interval_ms),
    })
}

// ── At (one-shot) ───────────────────────────────────────────────────

fn next_at_fire(expression: &str, now_ms: i64) -> Result<Option<i64>> {
    match parse_iso_ts(expression) {
        Some(target) if target > now_ms => Ok(Some(target)),
        Some(_) => Ok(None), // past — schedule exhausted
        None => bail!("invalid at expression (must be ISO 8601 timestamp): {}", expression),
    }
}

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

// ── Validation ──────────────────────────────────────────────────────

/// Validate a schedule expression parses correctly for the given type.
pub fn validate_expression(schedule_type: &str, expression: &str) -> Result<()> {
    match schedule_type {
        "cron" => {
            cron::Schedule::from_str(expression)
                .with_context(|| format!("invalid cron expression: {}", expression))?;
            Ok(())
        }
        "interval" => {
            let secs: i64 = expression.parse()
                .with_context(|| format!("interval must be integer seconds: {}", expression))?;
            if secs <= 0 {
                bail!("interval must be positive, got: {}", secs);
            }
            Ok(())
        }
        "at" => {
            if parse_iso_ts(expression).is_none() {
                bail!("invalid at expression (must be ISO 8601 timestamp): {}", expression);
            }
            Ok(())
        }
        other => bail!("unknown schedule_type: {}", other),
    }
}

/// Check if an `at` schedule's timestamp is in the past.
pub fn is_at_past(expression: &str, now_ms: i64) -> bool {
    parse_iso_ts(expression).is_none_or(|ts| ts <= now_ms)
}

/// Validate a timezone string.
pub fn validate_timezone(tz: &str) -> Result<()> {
    tz.parse::<chrono_tz::Tz>()
        .with_context(|| format!("invalid IANA timezone: {}", tz))?;
    Ok(())
}

/// Validate a schedule_id (no `/`, `..`, whitespace).
pub fn validate_schedule_id(id: &str) -> Result<()> {
    if id.is_empty() {
        bail!("schedule_id must not be empty");
    }
    if id.contains('/') || id.contains('\\') {
        bail!("schedule_id must not contain path separators: {}", id);
    }
    if id == "." || id == ".." {
        bail!("schedule_id must not be '.' or '..'");
    }
    if id.chars().any(|c| c.is_whitespace()) {
        bail!("schedule_id must not contain whitespace: {}", id);
    }
    Ok(())
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

    // ── compute_next_fire ────────────────────────────────────────

    #[test]
    fn next_interval_first_fire() {
        let now = 1000_000;
        let result = compute_next_fire("interval", "60", "UTC", now, None).unwrap();
        assert_eq!(result, Some(now + 60_000));
    }

    #[test]
    fn next_interval_subsequent_fire() {
        let now = 1000_000;
        let last = 900_000;
        let result = compute_next_fire("interval", "60", "UTC", now, Some(last)).unwrap();
        assert!(result.unwrap() > now);
    }

    #[test]
    fn next_at_future() {
        let now = 1000_000;
        let future = now + 600_000;
        let ts = chrono::DateTime::from_timestamp_millis(future)
            .unwrap()
            .to_rfc3339();
        let result = compute_next_fire("at", &ts, "UTC", now, None).unwrap();
        assert!(result.is_some());
    }

    #[test]
    fn next_at_past() {
        let now = 1800000000000; // ~2027 — well after 2020
        let past_ts = "2020-01-01T00:00:00Z";
        let result = compute_next_fire("at", past_ts, "UTC", now, None).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn next_cron_basic() {
        let now = chrono::Utc::now().timestamp_millis();
        let result = compute_next_fire("cron", "0 * * * * *", "UTC", now, None).unwrap();
        assert!(result.is_some());
        assert!(result.unwrap() > now);
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

    // ── is_at_past ───────────────────────────────────────────────

    #[test]
    fn at_is_past_old_timestamp() {
        assert!(is_at_past("2020-01-01T00:00:00Z", lillux::time::timestamp_millis()));
    }

    #[test]
    fn at_is_not_past_future_timestamp() {
        let future = chrono::DateTime::from_timestamp_millis(
            lillux::time::timestamp_millis() + 600_000,
        )
        .unwrap()
        .to_rfc3339();
        assert!(!is_at_past(&future, lillux::time::timestamp_millis()));
    }
}
