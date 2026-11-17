//! RFC 3339 timestamp parsing and formatting shared by the workspace.
//!
//! Timestamps go into signed and hashed records, so the byte output of the
//! formatters here is pinned by the tests below. Instants cross this API as
//! plain integers (Unix seconds, milliseconds or nanoseconds), never as a
//! date-time library type.

use anyhow::{Result, anyhow};

const NANOS_PER_SECOND: i128 = 1_000_000_000;
const NANOS_PER_MILLI: i128 = 1_000_000;

fn parse_fixed(value: &str) -> Result<chrono::DateTime<chrono::FixedOffset>> {
    chrono::DateTime::parse_from_rfc3339(value)
        .map_err(|error| anyhow!("invalid RFC 3339 timestamp: {error}"))
}

/// Parse an RFC 3339 timestamp (any numeric offset, `Z`, optional fractional
/// seconds) into Unix nanoseconds.
pub fn parse_unix_nanos(value: &str) -> Result<i128> {
    let parsed = parse_fixed(value)?;
    Ok(i128::from(parsed.timestamp()) * NANOS_PER_SECOND
        + i128::from(parsed.timestamp_subsec_nanos()))
}

/// Unix milliseconds, rounded toward negative infinity.
pub fn parse_unix_millis(value: &str) -> Result<i64> {
    let millis = parse_unix_nanos(value)?.div_euclid(NANOS_PER_MILLI);
    i64::try_from(millis).map_err(|_| anyhow!("RFC 3339 timestamp is out of range"))
}

/// Unix seconds, rounded toward negative infinity.
pub fn parse_unix_seconds(value: &str) -> Result<i64> {
    let seconds = parse_unix_nanos(value)?.div_euclid(NANOS_PER_SECOND);
    i64::try_from(seconds).map_err(|_| anyhow!("RFC 3339 timestamp is out of range"))
}

/// `YYYY-MM-DDTHH:MM:SS.mmmZ` for Unix milliseconds.
pub fn format_unix_millis(millis: i64) -> Result<String> {
    Ok(chrono::DateTime::from_timestamp_millis(millis)
        .ok_or_else(|| anyhow!("Unix milliseconds are out of range: {millis}"))?
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
}

/// `YYYY-MM-DDTHH:MM:SSZ` for Unix seconds.
pub fn format_unix_seconds(seconds: i64) -> Result<String> {
    Ok(chrono::DateTime::from_timestamp(seconds, 0)
        .ok_or_else(|| anyhow!("Unix seconds are out of range: {seconds}"))?
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
}

/// Re-spell a parsed timestamp as UTC milliseconds with `Z` (sub-millisecond
/// digits are truncated).
pub fn normalize_millis(value: &str) -> Result<String> {
    format_unix_millis(parse_unix_millis(value)?)
}

/// Re-spell a parsed timestamp as UTC whole seconds with `Z` (fractional
/// seconds are truncated).
pub fn normalize_seconds(value: &str) -> Result<String> {
    format_unix_seconds(parse_unix_seconds(value)?)
}

/// The wall clock now, as UTC milliseconds with `Z`.
pub fn now_millis() -> String {
    format_unix_millis(lillux::time::timestamp_millis()).expect("wall clock is within range")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_are_byte_pinned() {
        assert_eq!(format_unix_millis(0).unwrap(), "1970-01-01T00:00:00.000Z");
        assert_eq!(
            format_unix_millis(1_700_000_000_123).unwrap(),
            "2023-11-14T22:13:20.123Z"
        );
        assert_eq!(
            format_unix_millis(1_700_000_000_005).unwrap(),
            "2023-11-14T22:13:20.005Z"
        );
        assert_eq!(format_unix_millis(-1).unwrap(), "1969-12-31T23:59:59.999Z");
        assert_eq!(
            format_unix_millis(253_402_300_799_999).unwrap(),
            "9999-12-31T23:59:59.999Z"
        );
        assert_eq!(format_unix_seconds(0).unwrap(), "1970-01-01T00:00:00Z");
        assert_eq!(
            format_unix_seconds(1_700_000_000).unwrap(),
            "2023-11-14T22:13:20Z"
        );
        assert_eq!(
            format_unix_seconds(253_402_300_799).unwrap(),
            "9999-12-31T23:59:59Z"
        );
        assert_eq!(
            format_unix_seconds(951_782_400).unwrap(),
            "2000-02-29T00:00:00Z"
        );
    }

    #[test]
    fn now_has_the_fixed_millisecond_shape() {
        let now = now_millis();
        assert_eq!(now.len(), "2026-08-11T00:00:00.000Z".len());
        assert!(now.ends_with('Z'));
        assert_eq!(normalize_millis(&now).unwrap(), now);
    }

    #[test]
    fn parses_offsets_and_z_to_the_same_instant() {
        let z = parse_unix_nanos("2026-11-15T00:10:00Z").unwrap();
        assert_eq!(z, 1794701400_000_000_000_i128);
        assert_eq!(parse_unix_nanos("2026-11-15T00:10:00+00:00").unwrap(), z);
        assert_eq!(parse_unix_nanos("2026-11-15T00:10:00-00:00").unwrap(), z);
        assert_eq!(parse_unix_nanos("2026-11-15T13:10:00+13:00").unwrap(), z);
        assert_eq!(parse_unix_nanos("2026-11-14T19:10:00-05:00").unwrap(), z);
        assert_eq!(parse_unix_nanos("2026-11-15T05:40:00+05:30").unwrap(), z);
    }

    #[test]
    fn fractional_seconds_parse_and_truncate_downward() {
        assert_eq!(
            parse_unix_millis("1970-01-01T00:00:01.123456789Z").unwrap(),
            1_123
        );
        assert_eq!(
            parse_unix_nanos("1970-01-01T00:00:01.5Z").unwrap(),
            1_500_000_000
        );
        assert_eq!(
            parse_unix_nanos("1970-01-01T00:00:01.123456789Z").unwrap(),
            1_123_456_789
        );
        assert_eq!(parse_unix_millis("1969-12-31T23:59:59.9995Z").unwrap(), -1);
        assert_eq!(parse_unix_seconds("1969-12-31T23:59:59.9Z").unwrap(), -1);
        assert_eq!(
            normalize_millis("2026-11-15T00:10:00.123999+00:00").unwrap(),
            "2026-11-15T00:10:00.123Z"
        );
        assert_eq!(
            normalize_seconds("2026-11-15T00:10:00.999+00:00").unwrap(),
            "2026-11-15T00:10:00Z"
        );
        assert_eq!(
            normalize_millis("2026-11-15T13:10:00.5+13:00").unwrap(),
            "2026-11-15T00:10:00.500Z"
        );
    }

    #[test]
    fn rejected_inputs_are_pinned() {
        for invalid in [
            "",
            "2026-11-15",
            "2026-11-15T00:10:00",
            "2026-11-15T00:10Z",
            "2026-11-15T00:10:00 Z",
            " 2026-11-15T00:10:00Z",
            "2026-11-15T00:10:00Z ",
            "2026-11-15T00:10:00+0000",
            "2026-11-15T00:10:00+00",
            "2026-11-15T00:10:00.Z",
            "2026-13-15T00:10:00Z",
            "2026-02-30T00:10:00Z",
            "2025-02-29T00:10:00Z",
            "2026-11-15T24:00:00Z",
            "2026-11-15T00:60:00Z",
            "2026-11-15T00:10:61Z",
            "26-11-15T00:10:00Z",
            "2026-11-15T00:10:00+24:00",
            "2026-11-15T00:10:00+00:60",
        ] {
            assert!(parse_unix_nanos(invalid).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn leap_year_dates() {
        assert!(parse_unix_nanos("2024-02-29T00:00:00Z").is_ok());
        assert!(parse_unix_nanos("2000-02-29T00:00:00Z").is_ok());
        assert!(parse_unix_nanos("1900-02-29T00:00:00Z").is_err());
    }

    #[test]
    fn range_edges() {
        assert_eq!(parse_unix_seconds("1970-01-01T00:00:00Z").unwrap(), 0);
        assert_eq!(parse_unix_seconds("1969-12-31T23:59:59Z").unwrap(), -1);
        assert_eq!(
            parse_unix_seconds("9999-12-31T23:59:59Z").unwrap(),
            253_402_300_799
        );
        assert_eq!(
            parse_unix_seconds("0001-01-01T00:00:00Z").unwrap(),
            -62_135_596_800
        );
    }

    // Behaviours that differ between date-time libraries; each one below is
    // pinned to the value the pre-migration (chrono) code produced.
    #[test]
    fn grammar_corners_are_pinned() {
        let accepts = |value: &str| parse_unix_nanos(value).is_ok();
        // Lowercase separators.
        assert!(accepts("2026-11-15T00:10:00z"));
        assert!(accepts("2026-11-15t00:10:00Z"));
        // Space separator.
        assert!(accepts("2026-11-15 00:10:00Z"));
        // Leap second.
        assert!(accepts("2026-06-30T23:59:60Z"));
        // More than nine fractional digits.
        assert!(accepts("2026-11-15T00:10:00.1234567890Z"));
        // Large offsets.
        assert!(accepts("2026-11-15T00:10:00+23:59"));
        // Seconds in offsets.
        assert!(!accepts("2026-11-15T00:10:00+00:00:30"));
    }
}
