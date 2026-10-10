//! Cron schedules for hub timers: the legacy automations' `cron` schedule kind.
//!
//! Parse an expression once, then arm the next occurrence with [`Ctx::timer_cron`] (or
//! [`Cron::next_after`] and [`Ctx::timer_after`]) when the hub starts and again in
//! [`Hub::on_timer`](crate::Hub::on_timer). A run that came due while the hub was stopped fires
//! once when it is back, and the next one is counted from then: missed runs are skipped, not
//! made up, as legacy's dispatcher skipped them.
//!
//! ```ignore
//! const RESTOCK: &str = "0 */6 * * *";
//!
//! fn spawn(ctx: &Ctx, _seed: &[u8]) -> Result<Self> {
//!     ctx.timer_cron("restock", &Cron::parse(RESTOCK)?)?;
//!     Ok(Self::default())
//! }
//!
//! fn on_timer(&mut self, ctx: &Ctx, timer: &str) -> Result<()> {
//!     if timer == "restock" {
//!         self.restock();
//!         ctx.timer_cron("restock", &Cron::parse(RESTOCK)?)?;
//!     }
//!     Ok(())
//! }
//! ```
//!
//! # Syntax
//!
//! What legacy automations accepted (they used cron-parser 4.9), matched the same way:
//!
//! - Five fields, `minute hour day-of-month month day-of-week`, or six with `second` first.
//!   Fewer than five leave the leading fields at their defaults (second `0`, the rest `*`), so
//!   the last field is always the day of the week.
//! - `*`, `?` (day of month and day of week only; the same as `*`), lists `a,b`, ranges `a-b`,
//!   steps `*/n`, `a-b/n` and `a/n` (from `a` to the field's end).
//! - Ranges: second and minute 0-59, hour 0-23, day of month 1-31, month 1-12, day of week 0-7
//!   (0 and 7 are Sunday). Month and day names (`JAN`-`DEC`, `SUN`-`SAT`), any case.
//! - `L` in the day of month: its last day. `nL` in the day of week: the month's last such
//!   weekday (`5L`, the last Friday). `n#k` as the whole day-of-week field: the k-th such
//!   weekday of the month (`1#2`, the second Monday; k 1-5).
//! - `@yearly`, `@monthly`, `@weekly`, `@daily` and `@hourly`.
//! - When both the day of month and the day of week are restricted, a day matches either one:
//!   `0 0 13 * 5` runs on the 13th and on every Friday. A field counts as unrestricted when it
//!   lists every value, as legacy decided it: `*`, `?`, `1-7` or `0-7` in the day of week (but
//!   not `0-6`), and in the day of month any list as long as the month has days (`1-30` is
//!   unrestricted in April and restricted in May). With one month named, days past its end are
//!   dropped, and a first day past it is refused (`* * 30 2 *`).
//!
//! Times are UTC unless [`Cron::with_utc_offset`] names a fixed offset. Legacy computed in the
//! API host's zone, UTC on every tier, and had no per-schedule zone; there are no named time
//! zones or daylight saving here.
//!
//! Stricter than legacy: an empty expression, a step on `L` (`L/2`), `15L` in the day of month,
//! a range with two dashes and `L` alone in the day of week are refused. Legacy took the first
//! four without complaint and failed the last at its first run.

use std::fmt;
use std::str::FromStr;

use crate::{Ctx, Error, Result};

/// A parsed cron expression. See the [module docs](self) for the syntax.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cron {
    source: String,
    seconds: u64,
    minutes: u64,
    hours: u32,
    /// Days of the month, bits 1-31.
    days: u32,
    /// `L` in the day of month.
    last_day: bool,
    /// Entries in the day-of-month field as legacy counted them, which decides whether it is
    /// unrestricted.
    day_entries: usize,
    /// Months, bits 1-12.
    months: u16,
    /// Days of the week, bits 0-6 (Sunday 0).
    weekdays: u8,
    /// `nL`: the last such weekday of the month.
    last_weekdays: u8,
    weekday_entries: usize,
    /// `n#k`: only the k-th week of the month (days 7k-6 to 7k); 0 when not given.
    nth: u8,
    /// Seconds east of UTC.
    offset_s: i64,
}

/// Why an expression was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronError(pub String);

impl fmt::Display for CronError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CronError {}

impl From<CronError> for Error {
    fn from(e: CronError) -> Self {
        Error(e.0)
    }
}

impl fmt::Display for Cron {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.source)
    }
}

impl FromStr for Cron {
    type Err = CronError;

    fn from_str(s: &str) -> Result<Self, CronError> {
        Cron::parse(s)
    }
}

/// Days in each month as legacy counted them for the unrestricted-day test: February has 29.
const LEGACY_MONTH_DAYS: [u32; 12] = [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
/// The first occurrence is looked for this far ahead: the Gregorian calendar repeats every 400
/// years, so an expression that has not matched by then never will.
const SEARCH_MONTHS: u32 = 400 * 12 + 1;
const MAX_OFFSET_MINUTES: i32 = 18 * 60;

const PREDEFINED: [(&str, &str); 5] =
    [("@yearly", "0 0 1 1 *"), ("@monthly", "0 0 1 * *"), ("@weekly", "0 0 * * 0"), ("@daily", "0 0 * * *"), ("@hourly", "0 * * * *")];
const MONTH_NAMES: [&str; 12] = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];
const DAY_NAMES: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Second,
    Minute,
    Hour,
    Day,
    Month,
    Weekday,
}

impl Field {
    const ALL: [Field; 6] = [Field::Second, Field::Minute, Field::Hour, Field::Day, Field::Month, Field::Weekday];

    fn range(self) -> (u32, u32) {
        match self {
            Field::Second | Field::Minute => (0, 59),
            Field::Hour => (0, 23),
            Field::Day => (1, 31),
            Field::Month => (1, 12),
            Field::Weekday => (0, 7),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Field::Second => "second",
            Field::Minute => "minute",
            Field::Hour => "hour",
            Field::Day => "day of month",
            Field::Month => "month",
            Field::Weekday => "day of week",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Value {
    N(u32),
    /// `L` in the day of month.
    Last,
    /// `nL` in the day of week.
    LastOf(u32),
}

fn refuse(msg: impl Into<String>) -> CronError {
    CronError(msg.into())
}

fn number(field: Field, s: &str) -> Result<u32, CronError> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) || s.len() > 9 {
        return Err(refuse(format!("{}: `{s}` is not a number", field.name())));
    }
    let v: u32 = s.parse().map_err(|_| refuse(format!("{}: `{s}` is not a number", field.name())))?;
    let (min, max) = field.range();
    if v < min || v > max {
        return Err(refuse(format!("{}: {v} is outside {min}-{max}", field.name())));
    }
    Ok(v)
}

/// Replaces month or day names with their numbers, as legacy did: any run of three letters.
fn replace_names(field: Field, raw: &str) -> Result<String, CronError> {
    let names: &[&str] = match field {
        Field::Month => &MONTH_NAMES,
        Field::Weekday => &DAY_NAMES,
        _ => return Ok(raw.to_owned()),
    };
    let base = if field == Field::Month { 1 } else { 0 };
    let mut out = String::with_capacity(raw.len());
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if i + 3 <= bytes.len() && bytes[i..i + 3].iter().all(u8::is_ascii_alphabetic) {
            let word = raw[i..i + 3].to_ascii_lowercase();
            let n = names.iter().position(|n| *n == word).ok_or_else(|| refuse(format!("{}: unknown name `{word}`", field.name())))?;
            out.push_str(&(n + base).to_string());
            i += 3;
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    Ok(out)
}

/// One field's values in legacy's order and multiplicity: the entry count matters.
fn parse_field(field: Field, raw: &str) -> Result<Vec<Value>, CronError> {
    let text = replace_names(field, raw)?;
    let allowed = |c: char| match field {
        Field::Day | Field::Weekday => c.is_ascii_digit() || ",*?L/-".contains(c),
        _ => c.is_ascii_digit() || ",*/-".contains(c),
    };
    if let Some(c) = text.chars().find(|c| !allowed(*c)) {
        return Err(refuse(format!("{}: `{c}` is not allowed in `{raw}`", field.name())));
    }
    let (min, max) = field.range();
    let text = if text.contains('*') {
        text.replace('*', &format!("{min}-{max}"))
    } else if text.contains('?') {
        text.replace('?', &format!("{min}-{max}"))
    } else {
        text
    };
    let mut out = Vec::new();
    for item in text.split(',') {
        if item.is_empty() {
            return Err(refuse(format!("{}: an empty item in `{raw}`", field.name())));
        }
        parse_item(field, item, &mut out)?;
    }
    Ok(out)
}

fn parse_item(field: Field, item: &str, out: &mut Vec<Value>) -> Result<(), CronError> {
    let (_, max) = field.range();
    let (base, step) = match item.split_once('/') {
        None => (item, None),
        Some((base, step)) => {
            if step.contains('/') {
                return Err(refuse(format!("{}: `{item}` has two steps", field.name())));
            }
            let step: u32 = match step.parse() {
                Ok(n) if n > 0 && step.bytes().all(|b| b.is_ascii_digit()) => n,
                _ => return Err(refuse(format!("{}: `{step}` is not a step", field.name()))),
            };
            (base, Some(step))
        }
    };
    if let Some(step) = step {
        // `a/n` runs from a to the field's end.
        let (lo, hi) = match base.split_once('-') {
            Some((lo, hi)) => (lo, hi),
            None if !base.is_empty() && base.bytes().all(|b| b.is_ascii_digit()) => (base, ""),
            None => return Err(refuse(format!("{}: a step needs a number or a range, not `{base}`", field.name()))),
        };
        let hi = if hi.is_empty() && !base.contains('-') { max.to_string() } else { hi.to_owned() };
        return range(field, lo, &hi, step, out);
    }
    if let Some((lo, hi)) = base.split_once('-') {
        return range(field, lo, hi, 1, out);
    }
    match (field, base) {
        (Field::Day, "L") => out.push(Value::Last),
        (Field::Weekday, "L") => return Err(refuse("day of week: `L` needs a weekday, as in `5L`")),
        (Field::Weekday, s) if s.len() == 2 && s.ends_with('L') => out.push(Value::LastOf(number(field, &s[..1])? % 7)),
        (_, s) if s.contains('L') => return Err(refuse(format!("{}: `{s}` is not supported", field.name()))),
        (Field::Weekday, s) => out.push(Value::N(number(field, s)? % 7)),
        (_, s) => out.push(Value::N(number(field, s)?)),
    }
    Ok(())
}

/// `lo-hi` every `step`. As legacy did, a day-of-week range ending at 7 (or 0) also lists 0.
fn range(field: Field, lo: &str, hi: &str, step: u32, out: &mut Vec<Value>) -> Result<(), CronError> {
    if hi.contains('-') {
        return Err(refuse(format!("{}: `{lo}-{hi}` is not a range", field.name())));
    }
    let (lo, hi) = (number(field, lo)?, number(field, hi)?);
    if lo > hi {
        return Err(refuse(format!("{}: the range {lo}-{hi} runs backwards", field.name())));
    }
    let mut values = Vec::new();
    if field == Field::Weekday && hi.is_multiple_of(7) {
        values.push(0);
    }
    let mut count = step;
    for v in lo..=hi {
        if !values.contains(&v) && count.is_multiple_of(step) {
            count = 1;
            values.push(v);
        } else {
            count += 1;
        }
    }
    out.extend(values.into_iter().map(Value::N));
    Ok(())
}

fn mask(values: &[Value]) -> u64 {
    values.iter().fold(0, |m, v| match v {
        Value::N(n) => m | 1 << n,
        _ => m,
    })
}

impl Cron {
    /// Parses an expression; see the [module docs](self) for what is accepted.
    pub fn parse(expr: &str) -> Result<Self, CronError> {
        let trimmed = expr.trim();
        let text = PREDEFINED.iter().find(|(name, _)| *name == trimmed).map_or(trimmed, |(_, e)| e);
        let atoms: Vec<&str> = text.split_whitespace().collect();
        if atoms.is_empty() {
            return Err(refuse("the expression is empty"));
        }
        if atoms.len() > 6 {
            return Err(refuse(format!("{} fields; a cron expression has five, or six with seconds", atoms.len())));
        }
        let skip = 6 - atoms.len();
        let mut nth = 0u8;
        let mut fields: Vec<Vec<Value>> = Vec::with_capacity(6);
        for (i, field) in Field::ALL.into_iter().enumerate() {
            let raw = if i < skip { if i == 0 { "0" } else { "*" } } else { atoms[i - skip] };
            let raw = match (field, raw.split_once('#')) {
                (Field::Weekday, Some((day, k))) => {
                    if raw.contains(',') || raw.contains('/') || raw.contains('-') || k.contains('#') {
                        return Err(refuse(format!("day of week: `#` stands alone, as in `1#2`, not `{raw}`")));
                    }
                    nth = match k.parse::<u8>() {
                        Ok(n @ 1..=5) if k.bytes().all(|b| b.is_ascii_digit()) => n,
                        _ => return Err(refuse(format!("day of week: `#{k}` is not a week of the month (1-5)"))),
                    };
                    day
                }
                _ => raw,
            };
            fields.push(parse_field(field, raw)?);
        }
        let mut days = fields[3].clone();
        let months = &fields[4];
        if let [Value::N(m)] = months[..] {
            // One month named: its missing days go, and a first day past its end is refused.
            let last = LEGACY_MONTH_DAYS[m as usize - 1];
            if let Some(first) = days.iter().filter_map(|v| if let Value::N(d) = v { Some(*d) } else { None }).min()
                && first > last
            {
                return Err(refuse(format!("day of month: month {m} has no day {first}")));
            }
            days.retain(|v| !matches!(v, Value::N(d) if *d > last));
        }
        Ok(Self {
            source: expr.trim().to_owned(),
            seconds: mask(&fields[0]),
            minutes: mask(&fields[1]),
            hours: mask(&fields[2]) as u32,
            days: mask(&days) as u32,
            last_day: days.contains(&Value::Last),
            day_entries: days.len(),
            months: mask(months) as u16,
            weekdays: (mask(&fields[5]) & 0x7f) as u8,
            last_weekdays: fields[5].iter().fold(0, |m, v| if let Value::LastOf(d) = v { m | 1 << d } else { m }),
            weekday_entries: fields[5].len(),
            nth,
            offset_s: 0,
        })
    }

    /// The same schedule in a fixed offset from UTC, in minutes east (`-300` for UTC-5), within
    /// ±18 hours. There is no daylight saving: a zone that has it needs a new offset when its
    /// clocks change.
    pub fn with_utc_offset(mut self, minutes: i32) -> Result<Self, CronError> {
        if minutes.abs() > MAX_OFFSET_MINUTES {
            return Err(refuse(format!("a UTC offset is within ±{MAX_OFFSET_MINUTES} minutes, not {minutes}")));
        }
        self.offset_s = minutes as i64 * 60;
        Ok(self)
    }

    /// The first time it fires strictly after `after_ms` (Unix milliseconds), on a whole second;
    /// `None` when it never does.
    pub fn next_after(&self, after_ms: u64) -> Option<u64> {
        let start = (after_ms / 1000) as i64 + 1 + self.offset_s;
        let (mut year, mut month, mut day) = civil(start.div_euclid(86_400));
        let second_of_day = start.rem_euclid(86_400) as u32;
        let mut from = Some((second_of_day / 3600, second_of_day / 60 % 60, second_of_day % 60));
        for _ in 0..SEARCH_MONTHS {
            if self.months & 1 << month != 0 {
                let len = month_days(year, month);
                for d in day..=len {
                    if self.day_matches(year, month, d, len)
                        && let Some((h, m, s)) = self.time_from(from.unwrap_or((0, 0, 0)))
                    {
                        let local = days_from_civil(year, month, d) * 86_400 + (h * 3600 + m * 60 + s) as i64;
                        return u64::try_from(local - self.offset_s).ok().map(|t| t * 1000);
                    }
                    from = None;
                }
            }
            from = None;
            day = 1;
            (year, month) = if month == 12 { (year + 1, 1) } else { (year, month + 1) };
        }
        None
    }

    fn day_matches(&self, year: i64, month: u32, day: u32, len: u32) -> bool {
        let weekday = (days_from_civil(year, month, day) + 4).rem_euclid(7) as u32;
        let day_match = self.days & 1 << day != 0 || (self.last_day && day == len);
        let weekday_match = self.weekdays & 1 << weekday != 0 || (self.last_weekdays & 1 << weekday != 0 && day + 7 > len);
        let any_day = self.day_entries >= LEGACY_MONTH_DAYS[month as usize - 1] as usize;
        let any_weekday = self.weekday_entries == 8;
        if !day_match && (!weekday_match || any_weekday) {
            return false;
        }
        if any_day && !any_weekday && !weekday_match {
            return false;
        }
        self.nth == 0 || day.div_ceil(7) == self.nth as u32
    }

    /// The first matching time of day at or after `(hour, minute, second)`.
    fn time_from(&self, (h0, m0, s0): (u32, u32, u32)) -> Option<(u32, u32, u32)> {
        for h in (h0..24).filter(|h| self.hours & 1 << h != 0) {
            let m_from = if h == h0 { m0 } else { 0 };
            for m in (m_from..60).filter(|m| self.minutes & 1 << m != 0) {
                let s_from = if h == h0 && m == m0 { s0 } else { 0 };
                let left = self.seconds & (u64::MAX << s_from);
                if left != 0 {
                    return Some((h, m, left.trailing_zeros()));
                }
            }
        }
        None
    }
}

impl Ctx {
    /// Runs [`Hub::on_timer`](crate::Hub::on_timer) with `name` at the next time `cron` fires,
    /// replacing any timer of that name. Call it again from `on_timer` to keep the schedule.
    pub fn timer_cron(&self, name: &str, cron: &Cron) -> Result<()> {
        let now = self.now_ms();
        let next = cron.next_after(now).ok_or_else(|| Error(format!("`{cron}` never fires")))?;
        self.timer_after(name, next - now)
    }
}

fn is_leap(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn month_days(year: i64, month: u32) -> u32 {
    if month == 2 && !is_leap(year) { 28 } else { LEGACY_MONTH_DAYS[month as usize - 1] }
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's `days_from_civil`).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The date `days` after 1970-01-01.
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (yoe + era * 400 + i64::from(month <= 2), month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unix milliseconds of `YYYY-MM-DDTHH:MM:SS.mmmZ`.
    fn ms(iso: &str) -> u64 {
        let n = |r: std::ops::Range<usize>| iso[r].parse::<u32>().unwrap();
        let days = days_from_civil(n(0..4) as i64, n(5..7), n(8..10));
        let secs = days * 86_400 + (n(11..13) * 3600 + n(14..16) * 60 + n(17..19)) as i64;
        secs as u64 * 1000 + n(20..23) as u64
    }

    fn iso(ms: u64) -> String {
        let secs = (ms / 1000) as i64;
        let (y, mo, d) = civil(secs.div_euclid(86_400));
        let s = secs.rem_euclid(86_400);
        format!("{y:04}-{mo:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z", s / 3600, s / 60 % 60, s % 60, ms % 1000)
    }

    // Generated from legacy's cron-parser 4.9.0 (cks-game-api's `computeNextRunAt`) with TZ=UTC:
    // each expression, the instant it is asked from, and its next occurrences in order.
    const GOLDEN: &[(&str, &str, &[&str])] = &[
        ("* * * * *", "2026-09-26T12:00:00.000Z", &["2026-09-26T12:01:00.000Z", "2026-09-26T12:02:00.000Z", "2026-09-26T12:03:00.000Z"]),
        ("* * * * *", "2026-09-26T12:00:30.500Z", &["2026-09-26T12:01:00.000Z", "2026-09-26T12:02:00.000Z"]),
        (
            "*/15 * * * *",
            "2026-09-26T12:07:00.000Z",
            &["2026-09-26T12:15:00.000Z", "2026-09-26T12:30:00.000Z", "2026-09-26T12:45:00.000Z", "2026-09-26T13:00:00.000Z"],
        ),
        (
            "5/15 * * * *",
            "2026-09-26T12:00:00.000Z",
            &[
                "2026-09-26T12:05:00.000Z",
                "2026-09-26T12:20:00.000Z",
                "2026-09-26T12:35:00.000Z",
                "2026-09-26T12:50:00.000Z",
                "2026-09-26T13:05:00.000Z",
            ],
        ),
        (
            "*/10 * * * * *",
            "2026-09-26T12:00:05.000Z",
            &["2026-09-26T12:00:10.000Z", "2026-09-26T12:00:20.000Z", "2026-09-26T12:00:30.000Z"],
        ),
        ("15 10 * * * *", "2026-09-26T12:10:15.000Z", &["2026-09-26T13:10:15.000Z", "2026-09-26T14:10:15.000Z"]),
        ("0 0 1 * *", "2026-01-31T23:59:59.000Z", &["2026-02-01T00:00:00.000Z", "2026-03-01T00:00:00.000Z", "2026-04-01T00:00:00.000Z"]),
        (
            "0 0 31 * *",
            "2026-01-31T00:00:00.000Z",
            &["2026-03-31T00:00:00.000Z", "2026-05-31T00:00:00.000Z", "2026-07-31T00:00:00.000Z", "2026-08-31T00:00:00.000Z"],
        ),
        (
            "0 0 L * *",
            "2026-01-15T00:00:00.000Z",
            &["2026-01-31T00:00:00.000Z", "2026-02-28T00:00:00.000Z", "2026-03-31T00:00:00.000Z", "2026-04-30T00:00:00.000Z"],
        ),
        ("0 0 L * *", "2028-01-31T00:00:00.001Z", &["2028-02-29T00:00:00.000Z", "2028-03-31T00:00:00.000Z"]),
        ("0 0 29 2 *", "2026-03-01T00:00:00.000Z", &["2028-02-29T00:00:00.000Z", "2032-02-29T00:00:00.000Z"]),
        ("0 0 29 2 *", "2096-03-01T00:00:00.000Z", &["2104-02-29T00:00:00.000Z", "2108-02-29T00:00:00.000Z"]),
        (
            "0 0 13 * 5",
            "2026-09-26T00:00:00.000Z",
            &[
                "2026-10-02T00:00:00.000Z",
                "2026-10-09T00:00:00.000Z",
                "2026-10-13T00:00:00.000Z",
                "2026-10-16T00:00:00.000Z",
                "2026-10-23T00:00:00.000Z",
            ],
        ),
        ("0 0 13 * *", "2026-09-26T00:00:00.000Z", &["2026-10-13T00:00:00.000Z", "2026-11-13T00:00:00.000Z"]),
        ("0 0 * * 1", "2026-09-26T00:00:00.000Z", &["2026-09-28T00:00:00.000Z", "2026-10-05T00:00:00.000Z"]),
        ("0 0 1 * 0-6", "2026-09-26T00:00:00.000Z", &["2026-09-27T00:00:00.000Z", "2026-09-28T00:00:00.000Z", "2026-09-29T00:00:00.000Z"]),
        ("0 0 1 * 1-7", "2026-09-26T00:00:00.000Z", &["2026-10-01T00:00:00.000Z", "2026-11-01T00:00:00.000Z"]),
        ("0 0 1 * 0,1,2,3,4,5,6,7", "2026-09-26T00:00:00.000Z", &["2026-10-01T00:00:00.000Z", "2026-11-01T00:00:00.000Z"]),
        ("0 0 * * 5L", "2026-09-26T00:00:00.000Z", &["2026-10-30T00:00:00.000Z", "2026-11-27T00:00:00.000Z", "2026-12-25T00:00:00.000Z"]),
        (
            "0 0 * * 1,5L",
            "2026-09-26T00:00:00.000Z",
            &["2026-09-28T00:00:00.000Z", "2026-10-05T00:00:00.000Z", "2026-10-12T00:00:00.000Z", "2026-10-19T00:00:00.000Z"],
        ),
        ("0 0 * * 1#2", "2026-09-26T00:00:00.000Z", &["2026-10-12T00:00:00.000Z", "2026-11-09T00:00:00.000Z", "2026-12-14T00:00:00.000Z"]),
        ("0 0 15 * 1#2", "2026-09-26T00:00:00.000Z", &["2026-10-12T00:00:00.000Z", "2026-11-09T00:00:00.000Z"]),
        ("0 0 * * *#2", "2026-09-26T00:00:00.000Z", &["2026-10-08T00:00:00.000Z", "2026-10-09T00:00:00.000Z", "2026-10-10T00:00:00.000Z"]),
        (
            "0 9 * JAN-MAR mon-fri",
            "2026-09-26T00:00:00.000Z",
            &["2027-01-01T09:00:00.000Z", "2027-01-04T09:00:00.000Z", "2027-01-05T09:00:00.000Z"],
        ),
        ("0 0 * * 7", "2026-09-26T00:00:00.000Z", &["2026-09-27T00:00:00.000Z", "2026-10-04T00:00:00.000Z"]),
        ("0 0 * * SUN", "2026-09-26T00:00:00.000Z", &["2026-09-27T00:00:00.000Z", "2026-10-04T00:00:00.000Z"]),
        (
            "0 0 * * 2-7/2",
            "2026-09-26T00:00:00.000Z",
            &["2026-09-27T00:00:00.000Z", "2026-09-29T00:00:00.000Z", "2026-10-01T00:00:00.000Z", "2026-10-03T00:00:00.000Z"],
        ),
        (
            "30 4 1,15 * 5",
            "2026-09-26T00:00:00.000Z",
            &["2026-10-01T04:30:00.000Z", "2026-10-02T04:30:00.000Z", "2026-10-09T04:30:00.000Z", "2026-10-15T04:30:00.000Z"],
        ),
        ("0 12 * *", "2026-09-26T00:00:00.000Z", &["2026-10-12T00:00:00.000Z", "2026-10-12T00:01:00.000Z", "2026-10-12T00:02:00.000Z"]),
        ("0 0 * 2 *", "2027-02-27T00:00:00.000Z", &["2027-02-28T00:00:00.000Z", "2028-02-01T00:00:00.000Z", "2028-02-02T00:00:00.000Z"]),
        ("0 0 29-31 2 *", "2026-01-01T00:00:00.000Z", &["2028-02-29T00:00:00.000Z"]),
        (
            "0 0 1-30 * mon",
            "2026-03-28T00:00:00.000Z",
            &["2026-03-29T00:00:00.000Z", "2026-03-30T00:00:00.000Z", "2026-04-06T00:00:00.000Z", "2026-04-13T00:00:00.000Z"],
        ),
        ("0 0 12 ? * mon", "2026-09-26T00:00:00.000Z", &["2026-09-28T12:00:00.000Z", "2026-10-05T12:00:00.000Z"]),
        ("0 0 0 1 1 *", "2026-09-26T00:00:00.000Z", &["2027-01-01T00:00:00.000Z", "2028-01-01T00:00:00.000Z"]),
        ("@yearly", "2026-09-26T12:34:56.000Z", &["2027-01-01T00:00:00.000Z"]),
        ("@monthly", "2026-09-26T12:34:56.000Z", &["2026-10-01T00:00:00.000Z"]),
        ("@weekly", "2026-09-26T12:34:56.000Z", &["2026-09-27T00:00:00.000Z"]),
        ("@daily", "2026-09-26T12:34:56.000Z", &["2026-09-27T00:00:00.000Z"]),
        ("@hourly", "2026-09-26T12:34:56.000Z", &["2026-09-26T13:00:00.000Z"]),
        (
            "0 0-23/6 * * *",
            "2026-09-26T13:00:00.000Z",
            &["2026-09-26T18:00:00.000Z", "2026-09-27T00:00:00.000Z", "2026-09-27T06:00:00.000Z"],
        ),
        (
            "10-20/5,45 3 * * *",
            "2026-09-26T00:00:00.000Z",
            &["2026-09-26T03:10:00.000Z", "2026-09-26T03:15:00.000Z", "2026-09-26T03:20:00.000Z", "2026-09-26T03:45:00.000Z"],
        ),
        ("0 0 1 */3 *", "2026-09-26T00:00:00.000Z", &["2026-10-01T00:00:00.000Z", "2027-01-01T00:00:00.000Z", "2027-04-01T00:00:00.000Z"]),
    ];

    /// Refused by legacy, at parse or at its first run.
    const REFUSED: &[&str] = &[
        "* * * * * * *",
        "60 * * * *",
        "* 24 * * *",
        "* * 0 * *",
        "* * 32 * *",
        "* * * 13 *",
        "* * * * 8",
        "* * 30 2 *",
        "* * * foo *",
        "1, * * * *",
        "*/0 * * * *",
        "5-1 * * * *",
        "? * * * *",
        "* * * * L",
        "* * * * 1#6",
        "* * * * 1,2#2",
        "* * * * 1-2#2",
        "@annually",
        "* * * * 1#0",
    ];

    #[test]
    fn next_occurrences_match_legacy() {
        for (expr, from, want) in GOLDEN {
            let cron = Cron::parse(expr).unwrap_or_else(|e| panic!("{expr}: {e}"));
            let mut at = ms(from);
            for w in *want {
                let next = cron.next_after(at).unwrap_or_else(|| panic!("{expr} from {}: never", iso(at)));
                assert_eq!(iso(next), *w, "{expr} after {}", iso(at));
                at = next;
            }
        }
    }

    #[test]
    fn refuses_what_legacy_refused() {
        for expr in REFUSED {
            assert!(Cron::parse(expr).is_err(), "{expr} was accepted");
        }
    }

    #[test]
    fn refuses_what_legacy_took_by_accident() {
        for expr in ["", "   ", "0 0 L/2 * *", "0 0 15L * *", "0 0 1-2-3 * *", "* * * * * /5", "1/2/3 * * * *", "* * * * 10L"] {
            assert!(Cron::parse(expr).is_err(), "{expr:?} was accepted");
        }
    }

    #[test]
    fn strictly_after_on_whole_seconds() {
        let every_second = Cron::parse("* * * * * *").unwrap();
        assert_eq!(every_second.next_after(ms("2026-09-26T12:00:00.000Z")), Some(ms("2026-09-26T12:00:01.000Z")));
        assert_eq!(every_second.next_after(ms("2026-09-26T12:00:00.999Z")), Some(ms("2026-09-26T12:00:01.000Z")));
        let noon = Cron::parse("0 12 * * *").unwrap();
        assert_eq!(noon.next_after(ms("2026-09-26T11:59:59.999Z")), Some(ms("2026-09-26T12:00:00.000Z")));
        assert_eq!(noon.next_after(ms("2026-09-26T12:00:00.000Z")), Some(ms("2026-09-27T12:00:00.000Z")));
        assert_eq!(noon.next_after(ms("2026-12-31T12:00:00.000Z")), Some(ms("2027-01-01T12:00:00.000Z")));
    }

    #[test]
    fn a_fixed_offset_moves_the_wall_clock() {
        let nine = Cron::parse("0 9 * * mon-fri").unwrap().with_utc_offset(-300).unwrap();
        // Friday 13:59 UTC is 08:59 at UTC-5; the next weekday 09:00 there is 14:00 UTC.
        assert_eq!(nine.next_after(ms("2026-09-25T13:59:00.000Z")), Some(ms("2026-09-25T14:00:00.000Z")));
        // After it, Saturday and Sunday are skipped in local time.
        assert_eq!(nine.next_after(ms("2026-09-25T14:00:00.000Z")), Some(ms("2026-09-28T14:00:00.000Z")));
        // 23:30 at UTC+5:30 on the 31st is 18:00 UTC; the local date decides the day.
        let month_end = Cron::parse("30 23 L * *").unwrap().with_utc_offset(330).unwrap();
        assert_eq!(month_end.next_after(ms("2026-09-01T00:00:00.000Z")), Some(ms("2026-09-30T18:00:00.000Z")));
        assert!(Cron::parse("* * * * *").unwrap().with_utc_offset(18 * 60 + 1).is_err());
    }

    #[test]
    fn a_schedule_that_never_fires_says_so() {
        // Day 31 in months that have none, with the month named twice so legacy's check skips it.
        let never = Cron::parse("0 0 31 2,2 *").unwrap();
        assert_eq!(never.next_after(ms("2026-09-26T00:00:00.000Z")), None);
    }

    #[test]
    fn parses_from_str_and_shows_its_source() {
        let c: Cron = " 0 */6 * * * ".parse().unwrap();
        assert_eq!(c.to_string(), "0 */6 * * *");
        assert_eq!(c.next_after(ms("2026-09-26T05:00:00.000Z")), Some(ms("2026-09-26T06:00:00.000Z")));
    }

    #[test]
    fn calendar_round_trips() {
        for days in [-800_000i64, -1, 0, 1, 10_957, 20_000, 47_541, 800_000] {
            let (y, m, d) = civil(days);
            assert_eq!(days_from_civil(y, m, d), days);
        }
        assert_eq!(civil(0), (1970, 1, 1));
        assert_eq!(civil(days_from_civil(2100, 3, 1) - 1), (2100, 2, 28));
        assert_eq!(civil(days_from_civil(2000, 3, 1) - 1), (2000, 2, 29));
    }
}
