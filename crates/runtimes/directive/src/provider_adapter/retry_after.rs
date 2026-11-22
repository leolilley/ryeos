//! Parsing of the `Retry-After` response header into a wait in milliseconds.
//!
//! The header carries either an integer number of delta-seconds or an HTTP
//! date. A date is taken relative to the Lillux wall clock. Anything else
//! (negative, non-numeric, a malformed date, a date that is not in the future)
//! is `None`: an unparseable ask is never guessed at. A parsed wait saturates
//! at [`RETRY_AFTER_SATURATION_MS`].

/// One day. A provider asking for longer is treated as asking for one day.
pub const RETRY_AFTER_SATURATION_MS: u64 = 86_400_000;

/// The wait the header asks for, relative to `now_ms` (Unix milliseconds).
pub fn parse_retry_after(value: &str, now_ms: i64) -> Option<u64> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if value.bytes().all(|byte| byte.is_ascii_digit()) {
        // Digits only, so the sole failure is overflow, which saturates.
        let seconds = value.parse::<u64>().unwrap_or(u64::MAX);
        return Some(seconds.saturating_mul(1_000).min(RETRY_AFTER_SATURATION_MS));
    }
    let at_secs = http_date_unix_secs(value)?;
    let at_ms = i64::try_from(at_secs.checked_mul(1_000)?).ok()?;
    let wait = at_ms.checked_sub(now_ms)?;
    if wait <= 0 {
        return None;
    }
    Some(u64::try_from(wait).ok()?.min(RETRY_AFTER_SATURATION_MS))
}

/// Unix seconds of an IMF-fixdate (`Sun, 06 Nov 1994 08:49:37 GMT`), the
/// form HTTP servers are required to send. The weekday is not checked.
fn http_date_unix_secs(value: &str) -> Option<u64> {
    let mut parts = value.split_ascii_whitespace();
    let weekday = parts.next()?;
    let day = parts.next()?;
    let month = parts.next()?;
    let year = parts.next()?;
    let time = parts.next()?;
    let zone = parts.next()?;
    if parts.next().is_some() || !weekday.ends_with(',') || zone != "GMT" {
        return None;
    }
    let month = match month {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    if day.len() != 2 || year.len() != 4 || time.len() != 8 {
        return None;
    }
    // Lillux owns the calendar arithmetic and validates every field.
    lillux::time::unix_secs_from_iso8601(&format!("{year}-{month:02}-{day}T{time}Z"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW_MS: i64 = 784_111_777_000; // Sun, 06 Nov 1994 08:49:37 GMT

    #[test]
    fn parser_table() {
        let cases: &[(&str, Option<u64>)] = &[
            ("0", Some(0)),
            ("5", Some(5_000)),
            (" 120 ", Some(120_000)),
            ("86400", Some(86_400_000)),
            ("86401", Some(86_400_000)),
            ("99999999999999999999999999", Some(86_400_000)),
            ("-5", None),
            ("+5", None),
            ("1.5", None),
            ("soon", None),
            ("", None),
            ("   ", None),
            ("Sun, 06 Nov 1994 08:49:47 GMT", Some(10_000)),
            ("Sun, 06 Nov 1994 08:49:37 GMT", None),
            ("Sun, 06 Nov 1994 08:49:36 GMT", None),
            ("Sat, 05 Nov 1994 08:49:37 GMT", None),
            ("Sun, 06 Nov 2094 08:49:37 GMT", Some(86_400_000)),
            ("Sun, 06 Nov 1994 08:49:47 UTC", None),
            ("Sun, 31 Nov 1994 08:49:47 GMT", None),
            ("Sun, 06 Foo 1994 08:49:47 GMT", None),
            ("Sun 06 Nov 1994 08:49:47 GMT", None),
            ("06 Nov 1994 08:49:47 GMT", None),
        ];
        for (value, expected) in cases {
            assert_eq!(parse_retry_after(value, NOW_MS), *expected, "{value:?}");
        }
    }
}
