//! The cron evaluator: parse a schedule expression and compute fire times.
//!
//! Grammar (six whitespace-separated fields, seconds first):
//!
//! ```text
//! sec  min  hour  day-of-month  month  day-of-week
//! 0-59 0-59 0-23  1-31          1-12   1-7  (1 = Sunday .. 7 = Saturday)
//! ```
//!
//! Each field is a comma-separated list of items. An item is `*`, a number,
//! a range `a-b`, or one of those with a step: `*/n`, `a-b/n`, `a/n` (which
//! means `a-max/n`). Month and day-of-week fields also take English names
//! (`Jan`, `january`, `Mon`, `Thursday`, case-insensitive) as numbers or in
//! ranges. Day-of-month and day-of-week also take `?`, which means `*`.
//!
//! Day-of-month and day-of-week combine with AND: a day fires only when both
//! match. The years 1970 to 2100 are searched, so nothing fires after 2100
//! and an impossible schedule such as `0 0 0 30 2 *` yields no fire.
//!
//! Time zones come from jiff's bundled IANA database, so results do not
//! depend on the host. A wall-clock time that does not exist (spring-forward
//! gap) is skipped. A wall-clock time that happens twice (fall-back) is
//! yielded twice by [`CronSchedule::boundaries_after`]; [`CronSchedule::next_after`]
//! walks wall-clock time from the given instant, so after the first of a
//! repeated pair it moves on to the next wall-clock match, and from inside
//! the second occurrence it returns the first (earlier than the given
//! instant). These quirks are inherited from the `cron` crate this replaced
//! and are pinned by the characterisation tests in `ryeos-scheduler`.
//!
//! Instants cross this API as milliseconds since the Unix epoch. Fires land
//! on whole seconds.

use anyhow::{Result, bail};
use jiff::SignedDuration;
use jiff::Timestamp;
use jiff::civil::{Date, DateTime};
use jiff::tz::{AmbiguousOffset, TimeZone};

/// First and last year searched; the same range the `cron` crate used.
const FIRST_YEAR: i16 = 1970;
const LAST_YEAR: i16 = 2100;

/// A parsed cron expression. Each field is a bit set: bit `n` set means
/// value `n` matches.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CronSchedule {
    seconds: u64,
    minutes: u64,
    hours: u64,
    days_of_month: u64,
    months: u64,
    /// 1 = Sunday .. 7 = Saturday.
    days_of_week: u64,
}

/// A validated IANA time zone from the bundled database.
#[derive(Clone, Debug)]
pub struct ScheduleZone(TimeZone);

impl ScheduleZone {
    /// Look up an IANA zone name. Names are case-sensitive, as they were
    /// under `chrono-tz`; `Etc/Unknown` is not a real zone and is refused.
    pub fn parse(name: &str) -> Result<Self> {
        let found = TimeZone::get(name).ok();
        match found {
            Some(tz) if tz.iana_name() == Some(name) && name != "Etc/Unknown" => Ok(Self(tz)),
            _ => bail!("invalid IANA timezone: {name}"),
        }
    }
}

impl CronSchedule {
    /// Parse a six-field expression, refusing anything outside the grammar.
    pub fn parse(expression: &str) -> Result<Self> {
        let fields: Vec<&str> = expression.split_whitespace().collect();
        let [sec, min, hour, dom, mon, dow] = fields[..] else {
            bail!(
                "cron expression must have 6 fields (sec min hour day-of-month month day-of-week), got {}: {expression}",
                fields.len()
            );
        };
        Ok(Self {
            seconds: parse_field(sec, &SECOND)?,
            minutes: parse_field(min, &MINUTE)?,
            hours: parse_field(hour, &HOUR)?,
            days_of_month: parse_field(dom, &DAY_OF_MONTH)?,
            months: parse_field(mon, &MONTH)?,
            days_of_week: parse_field(dow, &DAY_OF_WEEK)?,
        })
    }

    /// The first fire strictly after `after_ms`, or `None` if there is none
    /// (or `after_ms` is outside the representable range).
    ///
    /// Inside a repeated fall-back hour this returns the earlier of a
    /// repeated pair only; see the module docs.
    pub fn next_after(&self, zone: &ScheduleZone, after_ms: i64) -> Option<i64> {
        self.next_hit(zone, after_ms).map(|hit| hit.first)
    }

    /// Every fire strictly after `after_ms`, in order. A repeated wall-clock
    /// time yields both instants.
    pub fn boundaries_after(&self, zone: &ScheduleZone, after_ms: i64) -> Boundaries<'_> {
        Boundaries {
            schedule: self,
            zone: zone.clone(),
            previous: Some(after_ms),
            pending: None,
        }
    }

    /// The next matching wall-clock time at or after one second past
    /// `after_ms`, resolved in `zone`.
    fn next_hit(&self, zone: &ScheduleZone, after_ms: i64) -> Option<Hit> {
        let tz = &zone.0;
        let start = Timestamp::from_millisecond(after_ms)
            .ok()?
            .checked_add(SignedDuration::from_secs(1))
            .ok()?;
        let local = tz.to_datetime(start);
        // Sub-second part dropped: fires land on whole seconds.
        let mut from = DateTime::new(
            local.year(),
            local.month(),
            local.day(),
            local.hour(),
            local.minute(),
            local.second(),
            0,
        )
        .ok()?;
        loop {
            let candidate = self.next_civil(from)?;
            let resolved = tz.to_ambiguous_timestamp(candidate);
            match resolved.offset() {
                AmbiguousOffset::Unambiguous { .. } => {
                    return Some(Hit {
                        first: resolved.unambiguous().ok()?.as_millisecond(),
                        second: None,
                    });
                }
                AmbiguousOffset::Fold { .. } => {
                    return Some(Hit {
                        first: resolved.earlier().ok()?.as_millisecond(),
                        second: Some(resolved.later().ok()?.as_millisecond()),
                    });
                }
                // The wall-clock time does not exist: skip it.
                AmbiguousOffset::Gap { .. } => {
                    from = candidate.checked_add(SignedDuration::from_secs(1)).ok()?;
                }
            }
        }
    }

    /// The earliest wall-clock time at or after `from` whose fields all
    /// match, ignoring time zones. Nested search from year down to second;
    /// a lower bound applies only while still inside `from`'s own
    /// year, month, day, hour and minute.
    fn next_civil(&self, from: DateTime) -> Option<DateTime> {
        let (fy, fmo, fd) = (from.year(), from.month(), from.day());
        let (fh, fmi, fs) = (from.hour(), from.minute(), from.second());
        for year in fy.max(FIRST_YEAR)..=LAST_YEAR {
            let in_year = year == fy;
            for month in (if in_year { fmo } else { 1 })..=12 {
                if !has(self.months, month) {
                    continue;
                }
                let in_month = in_year && month == fmo;
                let days = Date::new(year, month, 1).ok()?.days_in_month();
                for day in (if in_month { fd } else { 1 })..=days {
                    if !has(self.days_of_month, day) {
                        continue;
                    }
                    let weekday = Date::new(year, month, day).ok()?.weekday();
                    if !has(self.days_of_week, weekday.to_sunday_one_offset()) {
                        continue;
                    }
                    let in_day = in_month && day == fd;
                    for hour in (if in_day { fh } else { 0 })..=23 {
                        if !has(self.hours, hour) {
                            continue;
                        }
                        let in_hour = in_day && hour == fh;
                        for minute in (if in_hour { fmi } else { 0 })..=59 {
                            if !has(self.minutes, minute) {
                                continue;
                            }
                            let in_minute = in_hour && minute == fmi;
                            for second in (if in_minute { fs } else { 0 })..=59 {
                                if has(self.seconds, second) {
                                    return DateTime::new(
                                        year, month, day, hour, minute, second, 0,
                                    )
                                    .ok();
                                }
                            }
                        }
                    }
                }
            }
        }
        None
    }
}

/// Successive fires of a schedule; see [`CronSchedule::boundaries_after`].
pub struct Boundaries<'a> {
    schedule: &'a CronSchedule,
    zone: ScheduleZone,
    /// The last instant yielded (or the start); `None` once exhausted.
    previous: Option<i64>,
    /// The second instant of a repeated pair, yielded next.
    pending: Option<i64>,
}

impl Iterator for Boundaries<'_> {
    type Item = i64;

    fn next(&mut self) -> Option<i64> {
        let previous = self.previous?;
        if let Some(later) = self.pending.take() {
            self.previous = Some(later);
            return Some(later);
        }
        let Some(hit) = self.schedule.next_hit(&self.zone, previous) else {
            self.previous = None;
            return None;
        };
        self.previous = Some(hit.first);
        self.pending = hit.second;
        Some(hit.first)
    }
}

/// One wall-clock match resolved to instants: a repeated time has two.
struct Hit {
    first: i64,
    second: Option<i64>,
}

fn has(set: u64, value: impl Into<i64>) -> bool {
    let value: i64 = value.into();
    (0..64).contains(&value) && set & (1u64 << value) != 0
}

// ── Field parsing ───────────────────────────────────────────────────

/// What one field accepts.
struct Unit {
    name: &'static str,
    min: u32,
    max: u32,
    /// English names and their numbers, lower case.
    names: &'static [(&'static str, u32)],
    /// Whether `?` is accepted (day-of-month and day-of-week only).
    allows_any: bool,
}

const SECOND: Unit = Unit {
    name: "seconds",
    min: 0,
    max: 59,
    names: &[],
    allows_any: false,
};
const MINUTE: Unit = Unit {
    name: "minutes",
    min: 0,
    max: 59,
    names: &[],
    allows_any: false,
};
const HOUR: Unit = Unit {
    name: "hours",
    min: 0,
    max: 23,
    names: &[],
    allows_any: false,
};
const DAY_OF_MONTH: Unit = Unit {
    name: "day-of-month",
    min: 1,
    max: 31,
    names: &[],
    allows_any: true,
};
const MONTH: Unit = Unit {
    name: "month",
    min: 1,
    max: 12,
    names: &[
        ("jan", 1),
        ("january", 1),
        ("feb", 2),
        ("february", 2),
        ("mar", 3),
        ("march", 3),
        ("apr", 4),
        ("april", 4),
        ("may", 5),
        ("jun", 6),
        ("june", 6),
        ("jul", 7),
        ("july", 7),
        ("aug", 8),
        ("august", 8),
        ("sep", 9),
        ("september", 9),
        ("oct", 10),
        ("october", 10),
        ("nov", 11),
        ("november", 11),
        ("dec", 12),
        ("december", 12),
    ],
    allows_any: false,
};
const DAY_OF_WEEK: Unit = Unit {
    name: "day-of-week",
    min: 1,
    max: 7,
    names: &[
        ("sun", 1),
        ("sunday", 1),
        ("mon", 2),
        ("monday", 2),
        ("tue", 3),
        ("tues", 3),
        ("tuesday", 3),
        ("wed", 4),
        ("wednesday", 4),
        ("thu", 5),
        ("thurs", 5),
        ("thursday", 5),
        ("fri", 6),
        ("friday", 6),
        ("sat", 7),
        ("saturday", 7),
    ],
    allows_any: true,
};

/// A number or a name, remembering which, so `1-Fri` can be refused.
#[derive(PartialEq, Clone, Copy)]
enum Kind {
    Number,
    Name,
}

fn parse_field(field: &str, unit: &Unit) -> Result<u64> {
    let mut set = 0u64;
    for item in field.split(',') {
        for value in parse_item(item, unit)? {
            set |= 1u64 << value;
        }
    }
    Ok(set)
}

/// The values one list item selects.
fn parse_item(item: &str, unit: &Unit) -> Result<Vec<u32>> {
    let (base, step) = match item.split_once('/') {
        Some((base, step)) => (base, Some(parse_number(step, unit)?)),
        None => (item, None),
    };
    if let Some(step) = step
        && !(1..=unit.max).contains(&step)
    {
        bail!(
            "{} step must be between 1 and {}: {item}",
            unit.name,
            unit.max
        );
    }
    let (start, end) = if base == "*" || (base == "?" && unit.allows_any) {
        (unit.min, unit.max)
    } else if let Some((a, b)) = base.split_once('-') {
        let (a, a_kind) = parse_value(a, unit)?;
        let (b, b_kind) = parse_value(b, unit)?;
        if a_kind != b_kind || a > b {
            bail!("invalid range for {}: {item}", unit.name);
        }
        (a, b)
    } else {
        let (value, value_kind) = parse_value(base, unit)?;
        // `a/n` runs from `a` to the field's maximum; a name cannot take a step.
        match (step, value_kind) {
            (None, _) => (value, value),
            (Some(_), Kind::Number) => (value, unit.max),
            (Some(_), Kind::Name) => bail!("a {} name cannot take a step: {item}", unit.name),
        }
    };
    Ok((start..=end).step_by(step.unwrap_or(1) as usize).collect())
}

/// A number or name within the field's range.
fn parse_value(text: &str, unit: &Unit) -> Result<(u32, Kind)> {
    let (value, kind) = if text.bytes().all(|b| b.is_ascii_digit()) && !text.is_empty() {
        (parse_number(text, unit)?, Kind::Number)
    } else {
        let lower = text.to_ascii_lowercase();
        match unit.names.iter().find(|(name, _)| *name == lower) {
            Some((_, number)) => (*number, Kind::Name),
            None => bail!("invalid {} value: {text:?}", unit.name),
        }
    };
    if !(unit.min..=unit.max).contains(&value) {
        bail!(
            "{} must be between {} and {}: {text}",
            unit.name,
            unit.min,
            unit.max
        );
    }
    Ok((value, kind))
}

fn parse_number(text: &str, unit: &Unit) -> Result<u32> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        bail!("invalid {} number: {text:?}", unit.name);
    }
    text.parse::<u32>()
        .map_err(|_| anyhow::anyhow!("{} number too large: {text}", unit.name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc() -> ScheduleZone {
        ScheduleZone::parse("UTC").unwrap()
    }

    #[test]
    fn impossible_schedule_terminates_without_a_fire() {
        let schedule = CronSchedule::parse("0 0 0 30 2 *").unwrap();
        assert_eq!(schedule.next_after(&utc(), 0), None);
        assert_eq!(schedule.boundaries_after(&utc(), 0).next(), None);
    }

    #[test]
    fn weekdays_count_from_sunday_one() {
        let sunday = CronSchedule::parse("0 0 0 * * 1").unwrap();
        // 2026-03-04 is a Wednesday; the next Sunday is 2026-03-08.
        let after = 1_772_619_630_000; // 2026-03-04T10:20:30Z
        assert_eq!(sunday.next_after(&utc(), after), Some(1_772_928_000_000));
        assert!(CronSchedule::parse("0 0 0 * * 0").is_err());
    }

    #[test]
    fn grammar_refusals() {
        for bad in [
            "* * * * *",
            "* * * * * * *",
            "@daily",
            "0 0 0 L * *",
            "0 0 0 * * 5#2",
            "*/0 * * * * *",
            "10-5 * * * * *",
            "? * * * * *",
        ] {
            assert!(CronSchedule::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn zone_names_are_case_sensitive() {
        assert!(ScheduleZone::parse("America/New_York").is_ok());
        assert!(ScheduleZone::parse("america/new_york").is_err());
        assert!(ScheduleZone::parse("Etc/Unknown").is_err());
    }
}
