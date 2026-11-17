//! RFC 3339 timestamp parsing and formatting shared by the workspace.
//!
//! Timestamps go into signed and hashed records, so the byte output of the
//! formatters here is pinned by the tests below. Instants cross this API as
//! plain integers (Unix seconds, milliseconds or nanoseconds), never as a
//! date-time library type.

use anyhow::{Result, anyhow};

const NANOS_PER_SECOND: i128 = 1_000_000_000;
const NANOS_PER_MILLI: i128 = 1_000_000;

const SECONDS_PER_DAY: i64 = 86_400;

/// `jiff::Timestamp` is not used for the instant itself: its range ends at
/// 9999-12-30T22:00:54Z, but authoritative state timestamps run through
/// 9999-12-31T23:59:59Z, and its grammar also accepts spellings (such as a
/// missing seconds field) that this contract has always refused. The shape is
/// therefore checked here and jiff supplies the calendar (date validity and
/// day arithmetic).
fn two_digits(bytes: &[u8], at: usize) -> Option<i64> {
    let (tens, ones) = (bytes.get(at)?, bytes.get(at + 1)?);
    (tens.is_ascii_digit() && ones.is_ascii_digit())
        .then(|| i64::from(tens - b'0') * 10 + i64::from(ones - b'0'))
}

fn parse_parts(value: &str) -> Option<i128> {
    let bytes = value.as_bytes();
    if bytes.len() < 20 || !bytes[..4].iter().all(u8::is_ascii_digit) {
        return None;
    }
    let year = bytes[..4]
        .iter()
        .fold(0_i16, |year, digit| year * 10 + i16::from(digit - b'0'));
    if bytes[4] != b'-'
        || bytes[7] != b'-'
        || !matches!(bytes[10], b'T' | b't' | b' ')
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return None;
    }
    let month = two_digits(bytes, 5)?;
    let day = two_digits(bytes, 8)?;
    let hour = two_digits(bytes, 11)?;
    let minute = two_digits(bytes, 14)?;
    let second = two_digits(bytes, 17)?;
    if hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let mut at = 19;
    let mut nanos: i128 = 0;
    if bytes[at] == b'.' {
        let digits = bytes[at + 1..]
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .count();
        if digits == 0 {
            return None;
        }
        for index in 0..9 {
            let digit = if index < digits {
                i128::from(bytes[at + 1 + index] - b'0')
            } else {
                0
            };
            nanos = nanos * 10 + digit;
        }
        at += 1 + digits;
    }
    let offset_seconds = match bytes.get(at..)? {
        [b'Z' | b'z'] => 0,
        [sign @ (b'+' | b'-'), ..] if bytes.len() == at + 6 && bytes[at + 3] == b':' => {
            let hours = two_digits(bytes, at + 1)?;
            let minutes = two_digits(bytes, at + 4)?;
            if hours > 23 || minutes > 59 {
                return None;
            }
            let magnitude = hours * 3_600 + minutes * 60;
            if *sign == b'-' { -magnitude } else { magnitude }
        }
        _ => return None,
    };
    let date =
        jiff::civil::Date::new(year, i8::try_from(month).ok()?, i8::try_from(day).ok()?).ok()?;
    let days = jiff::civil::Date::new(1970, 1, 1)
        .ok()?
        .until((jiff::Unit::Day, date))
        .ok()?
        .get_days();
    // A leap second (`:60`) is the first instant of the following second.
    let seconds =
        i64::from(days) * SECONDS_PER_DAY + hour * 3_600 + minute * 60 + second - offset_seconds;
    Some(i128::from(seconds) * NANOS_PER_SECOND + nanos)
}

/// Parse an RFC 3339 timestamp (any numeric offset, `Z`, optional fractional
/// seconds) into Unix nanoseconds.
pub fn parse_unix_nanos(value: &str) -> Result<i128> {
    parse_parts(value).ok_or_else(|| anyhow!("invalid RFC 3339 timestamp"))
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

/// Calendar date and time of day for Unix seconds, within jiff's civil range
/// (years -9999..=9999).
fn civil_utc(seconds: i64) -> Result<(jiff::civil::Date, i64)> {
    let out_of_range = || anyhow!("Unix time is out of range: {seconds}");
    let days = seconds.div_euclid(SECONDS_PER_DAY);
    let date = jiff::civil::Date::new(1970, 1, 1)
        .map_err(|_| out_of_range())?
        .checked_add(jiff::Span::new().days(days))
        .map_err(|_| out_of_range())?;
    Ok((date, seconds.rem_euclid(SECONDS_PER_DAY)))
}

/// `YYYY-MM-DDTHH:MM:SS.mmmZ` for Unix milliseconds.
pub fn format_unix_millis(millis: i64) -> Result<String> {
    let (date, second_of_day) = civil_utc(millis.div_euclid(1_000))?;
    Ok(format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        date.year(),
        date.month(),
        date.day(),
        second_of_day / 3_600,
        second_of_day % 3_600 / 60,
        second_of_day % 60,
        millis.rem_euclid(1_000)
    ))
}

/// `YYYY-MM-DDTHH:MM:SSZ` for Unix seconds.
pub fn format_unix_seconds(seconds: i64) -> Result<String> {
    let (date, second_of_day) = civil_utc(seconds)?;
    Ok(format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        date.year(),
        date.month(),
        date.day(),
        second_of_day / 3_600,
        second_of_day % 3_600 / 60,
        second_of_day % 60
    ))
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
    // pinned to the value the pre-migration (chrono) implementation produced.
    #[test]
    fn grammar_corners_are_pinned() {
        let accepts = |value: &str| parse_unix_nanos(value).is_ok();
        // Lowercase separators.
        assert!(accepts("2026-11-15T00:10:00z"));
        assert!(accepts("2026-11-15t00:10:00Z"));
        // Space separator.
        assert!(accepts("2026-11-15 00:10:00Z"));
        // Leap second: the first instant of the following second.
        assert!(accepts("2026-06-30T23:59:60Z"));
        assert_eq!(
            parse_unix_nanos("2026-06-30T23:59:60Z").unwrap(),
            parse_unix_nanos("2026-07-01T00:00:00Z").unwrap()
        );
        // More than nine fractional digits.
        assert!(accepts("2026-11-15T00:10:00.1234567890Z"));
        // Large offsets.
        assert!(accepts("2026-11-15T00:10:00+23:59"));
        // Seconds in offsets.
        assert!(!accepts("2026-11-15T00:10:00+00:00:30"));
    }
}
