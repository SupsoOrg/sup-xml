//! EXSLT date family — https://exslt.org/date/
//!
//! All functions live in the `http://exslt.org/dates-and-times`
//! namespace.  The implementation pairs a small hand-written XSD
//! lexical parser/formatter (sized to the EXSLT 1.0 subset:
//! dateTime / date / time / duration only) with `chrono` for the
//! actual calendar arithmetic — leap years, day-clipping when
//! adding months, day-of-week computation, etc.  Chrono is doing
//! the work that's been spec-debugged across millions of users;
//! we just bridge XSD↔chrono and dispatch by function name.
//!
//! Functions are *defensive*: malformed input → empty string
//! (not an error), matching libexslt's behaviour and the EXSLT
//! spec's "if the argument is not a valid xs:dateTime, returns an
//! empty string" pattern.  Missing-arg-uses-current-time is the
//! other defensive trick the spec uses.

use chrono::{
    DateTime, Datelike, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime,
    TimeZone, Timelike, Utc,
};

use crate::error::{ErrorDomain, ErrorLevel, XmlError};
use crate::xpath::eval::{Numeric, Value, value_to_string};
use crate::xpath::index::DocIndexLike;

use super::Result;

fn err(msg: impl Into<String>) -> XmlError {
    XmlError::new(ErrorDomain::XPath, ErrorLevel::Error, msg)
}

// ── EXSLT-internal datetime representation ────────────────────────
//
// XSD dateTime is "civil time plus an optional offset."  Chrono's
// `DateTime<FixedOffset>` carries the offset always; for values
// where XSD lets the offset be absent we use `NaiveDateTime` plus
// `None`.  Carrying the difference lets `format_*` emit the right
// shape when round-tripping (an XSD value without a timezone must
// not be re-emitted with one).

#[derive(Clone, Copy, Debug)]
struct Dt {
    naive:  NaiveDateTime,
    offset: Option<FixedOffset>,
}

impl Dt {
    /// UTC-anchor for cross-zone math.  Untimezoned values are
    /// treated as UTC (libexslt convention, EXSLT spec says
    /// implementation-defined).
    fn to_utc(self) -> DateTime<Utc> {
        let off = self.offset.unwrap_or(FixedOffset::east_opt(0).unwrap());
        // Build DateTime<FixedOffset>, then convert.
        off.from_local_datetime(&self.naive).single()
            .map(|dt| dt.with_timezone(&Utc))
            .unwrap_or_else(|| Utc.from_utc_datetime(&self.naive))
    }
}

// ── XSD lexical parsers ──────────────────────────────────────────

/// `xs:dateTime` lexical form:
///   `(-)? YYYY [Y*] - MM - DD T hh : mm : ss [. frac] [ Z | [+-]hh:mm ]?`
fn parse_xsd_datetime(s: &str) -> Option<Dt> {
    let (s, neg) = strip_neg(s);
    let (year_part, rest) = take_year(s)?;
    let rest = expect_char(rest, '-')?;
    let (month, rest) = take_n_digits(rest, 2)?;
    let rest = expect_char(rest, '-')?;
    let (day, rest) = take_n_digits(rest, 2)?;
    let rest = expect_char(rest, 'T')?;
    let (hour, rest) = take_n_digits(rest, 2)?;
    let rest = expect_char(rest, ':')?;
    let (minute, rest) = take_n_digits(rest, 2)?;
    let rest = expect_char(rest, ':')?;
    let (second, rest) = take_n_digits(rest, 2)?;
    let (nano, rest) = parse_optional_fraction(rest);
    let (offset, rest) = parse_optional_tz(rest);
    if !rest.is_empty() { return None; }

    let year = signed(year_part, neg)?;
    let date = NaiveDate::from_ymd_opt(year, month as u32, day as u32)?;
    let time = NaiveTime::from_hms_nano_opt(hour as u32, minute as u32, second as u32, nano)?;
    Some(Dt { naive: date.and_time(time), offset })
}

/// `xs:date` lexical form:  `(-)? YYYY [Y*] - MM - DD [TZ]?`
fn parse_xsd_date(s: &str) -> Option<Dt> {
    let (s, neg) = strip_neg(s);
    let (year_part, rest) = take_year(s)?;
    let rest = expect_char(rest, '-')?;
    let (month, rest) = take_n_digits(rest, 2)?;
    let rest = expect_char(rest, '-')?;
    let (day, rest) = take_n_digits(rest, 2)?;
    let (offset, rest) = parse_optional_tz(rest);
    if !rest.is_empty() { return None; }
    let year = signed(year_part, neg)?;
    let date = NaiveDate::from_ymd_opt(year, month as u32, day as u32)?;
    Some(Dt { naive: date.and_time(NaiveTime::MIN), offset })
}

/// `xs:time` lexical form: `hh:mm:ss[.frac][TZ]?`
fn parse_xsd_time(s: &str) -> Option<Dt> {
    let (hour, rest) = take_n_digits(s, 2)?;
    let rest = expect_char(rest, ':')?;
    let (minute, rest) = take_n_digits(rest, 2)?;
    let rest = expect_char(rest, ':')?;
    let (second, rest) = take_n_digits(rest, 2)?;
    let (nano, rest) = parse_optional_fraction(rest);
    let (offset, rest) = parse_optional_tz(rest);
    if !rest.is_empty() { return None; }
    let time = NaiveTime::from_hms_nano_opt(hour as u32, minute as u32, second as u32, nano)?;
    // Anchor time-only values to 1970-01-01 — EXSLT only cares
    // about the time fields; the date is never re-emitted from a
    // time-only parse.
    let date = NaiveDate::from_ymd_opt(1970, 1, 1)?;
    Some(Dt { naive: date.and_time(time), offset })
}

/// Try every XSD lexical shape we support, in order of specificity.
fn parse_any(s: &str) -> Option<Dt> {
    parse_xsd_datetime(s).or_else(|| parse_xsd_date(s)).or_else(|| parse_xsd_time(s))
}

// ── duration ──────────────────────────────────────────────────────
//
// XSD duration is a hybrid: (years, months) live in calendar
// space, (days, hours, minutes, seconds) live in fixed-elapsed
// time.  We can't normalise across the boundary without a
// reference date (e.g. "1 month" is 28-31 days).  Store both
// halves as i64 with seconds carrying the sub-second fractional
// part as nanos.

#[derive(Clone, Copy, Debug)]
struct XsdDuration {
    /// Sign applies uniformly across all components per XSD §3.2.6.
    negative: bool,
    months:   i64,   // years are folded into months at parse time
    days:     i64,
    seconds:  i64,
    nanos:    i64,
}

impl XsdDuration {
    /// `chrono::Months` for the calendar-month portion.
    fn months_part(&self) -> i64 {
        if self.negative { -self.months } else { self.months }
    }
    /// `chrono::Duration` for the day+time portion (fixed elapsed).
    fn duration_part(&self) -> chrono::Duration {
        let total_nanos =
            (self.days as i128) * 86_400 * 1_000_000_000
            + (self.seconds as i128) * 1_000_000_000
            + self.nanos as i128;
        let signed = if self.negative { -total_nanos } else { total_nanos };
        chrono::Duration::nanoseconds(signed.clamp(i64::MIN as i128, i64::MAX as i128) as i64)
    }
}

/// `xs:duration` lexical form:
///   `(-)? P (nY)? (nM)? (nD)? (T (nH)? (nM)? (nS | n.fracS))?`
fn parse_xsd_duration(s: &str) -> Option<XsdDuration> {
    let (mut s, neg) = strip_neg(s);
    s = s.strip_prefix('P')?;
    let mut years = 0i64;
    let mut months = 0i64;
    let mut days = 0i64;
    let mut hours = 0i64;
    let mut minutes = 0i64;
    let mut seconds = 0i64;
    let mut nanos = 0i64;
    let mut any = false;

    // Date portion: years, months, days.
    while let Some(c) = s.chars().next() {
        if c == 'T' { break; }
        let (n, rest) = take_unsigned_int(s)?;
        let unit = rest.chars().next()?;
        s = &rest[unit.len_utf8()..];
        match unit {
            'Y' => years = n as i64,
            'M' => months = n as i64,
            'D' => days = n as i64,
            _   => return None,
        }
        any = true;
    }
    // Time portion.
    if let Some(rest) = s.strip_prefix('T') {
        s = rest;
        if s.is_empty() { return None; }
        loop {
            if s.is_empty() { break; }
            let (n, mut rest) = take_unsigned_int(s)?;
            // Seconds can carry a fraction.
            let mut frac_nanos = 0i64;
            if let Some(after_dot) = rest.strip_prefix('.') {
                let (digits, after) = take_digits(after_dot);
                if digits.is_empty() { return None; }
                frac_nanos = digits_to_nanos(digits);
                rest = after;
            }
            let unit = rest.chars().next()?;
            s = &rest[unit.len_utf8()..];
            match unit {
                'H' => hours = n as i64,
                'M' => minutes = n as i64,
                'S' => { seconds = n as i64; nanos = frac_nanos; }
                _   => return None,
            }
            any = true;
        }
    }
    if !any || !s.is_empty() { return None; }
    let total_months = years.checked_mul(12)?.checked_add(months)?;
    let total_seconds = hours.checked_mul(3600)?
        .checked_add(minutes.checked_mul(60)?)?
        .checked_add(seconds)?;
    Some(XsdDuration {
        negative: neg, months: total_months, days, seconds: total_seconds, nanos,
    })
}

// ── XSD formatters ───────────────────────────────────────────────

fn format_xsd_datetime(dt: &Dt) -> String {
    let mut s = format_year(dt.naive.year());
    s.push_str(&format!(
        "-{:02}-{:02}T{:02}:{:02}:{:02}",
        dt.naive.month(), dt.naive.day(),
        dt.naive.hour(), dt.naive.minute(), dt.naive.second(),
    ));
    let nanos = dt.naive.nanosecond();
    if nanos != 0 {
        let frac = format!(".{:09}", nanos);
        s.push_str(frac.trim_end_matches('0'));
    }
    if let Some(off) = dt.offset { s.push_str(&format_offset(off)); }
    s
}

fn format_xsd_date(dt: &Dt) -> String {
    let mut s = format_year(dt.naive.year());
    s.push_str(&format!("-{:02}-{:02}", dt.naive.month(), dt.naive.day()));
    if let Some(off) = dt.offset { s.push_str(&format_offset(off)); }
    s
}

fn format_xsd_time(dt: &Dt) -> String {
    let mut s = format!(
        "{:02}:{:02}:{:02}",
        dt.naive.hour(), dt.naive.minute(), dt.naive.second(),
    );
    let nanos = dt.naive.nanosecond();
    if nanos != 0 {
        let frac = format!(".{:09}", nanos);
        s.push_str(frac.trim_end_matches('0'));
    }
    if let Some(off) = dt.offset { s.push_str(&format_offset(off)); }
    s
}

fn format_year(y: i32) -> String {
    if y < 0 { format!("-{:04}", -y) } else { format!("{:04}", y) }
}

fn format_offset(off: FixedOffset) -> String {
    let secs = off.local_minus_utc();
    if secs == 0 { return "Z".to_string(); }
    let (sign, mag) = if secs >= 0 { ('+', secs) } else { ('-', -secs) };
    format!("{sign}{:02}:{:02}", mag / 3600, (mag % 3600) / 60)
}

fn format_xsd_duration(d: &XsdDuration) -> String {
    // XSD §3.2.6: "PnYnMnDTnHnMnS" — components with value 0 are
    // omitted; if all are 0, the duration is "PT0S".
    let mut s = String::new();
    if d.negative { s.push('-'); }
    s.push('P');
    let mut any_date = false;
    let years = d.months / 12;
    let months = d.months % 12;
    if years  != 0 { s.push_str(&format!("{years}Y"));  any_date = true; }
    if months != 0 { s.push_str(&format!("{months}M")); any_date = true; }
    if d.days != 0 { s.push_str(&format!("{}D", d.days)); any_date = true; }
    let has_time = d.seconds != 0 || d.nanos != 0;
    if has_time {
        s.push('T');
        let hours   = d.seconds / 3600;
        let minutes = (d.seconds % 3600) / 60;
        let secs    = d.seconds % 60;
        if hours   != 0 { s.push_str(&format!("{hours}H"));   }
        if minutes != 0 { s.push_str(&format!("{minutes}M")); }
        if secs != 0 || d.nanos != 0 {
            if d.nanos == 0 {
                s.push_str(&format!("{secs}S"));
            } else {
                let frac = format!(".{:09}", d.nanos);
                s.push_str(&format!("{secs}{}S", frac.trim_end_matches('0')));
            }
        }
    } else if !any_date {
        s.push_str("PT0S".strip_prefix('P').unwrap());
    }
    s
}

// ── parser primitives ────────────────────────────────────────────

fn strip_neg(s: &str) -> (&str, bool) {
    match s.strip_prefix('-') { Some(rest) => (rest, true), None => (s, false) }
}

/// XSD year is 4+ digits, no leading zeros once past the first 4.
fn take_year(s: &str) -> Option<(&str, &str)> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() { i += 1; }
    if i < 4 { return None; }
    Some((&s[..i], &s[i..]))
}

fn take_n_digits(s: &str, n: usize) -> Option<(u32, &str)> {
    if s.len() < n { return None; }
    let head = &s[..n];
    if !head.bytes().all(|b| b.is_ascii_digit()) { return None; }
    Some((head.parse().ok()?, &s[n..]))
}

fn take_digits(s: &str) -> (&str, &str) {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() { i += 1; }
    (&s[..i], &s[i..])
}

fn take_unsigned_int(s: &str) -> Option<(u64, &str)> {
    let (head, rest) = take_digits(s);
    if head.is_empty() { return None; }
    Some((head.parse().ok()?, rest))
}

fn expect_char(s: &str, c: char) -> Option<&str> {
    s.strip_prefix(c)
}

fn signed(year_part: &str, neg: bool) -> Option<i32> {
    let y: i32 = year_part.parse().ok()?;
    if y == 0 { return None; }   // XSD: year 0 illegal
    Some(if neg { -y } else { y })
}

fn parse_optional_fraction(s: &str) -> (u32, &str) {
    match s.strip_prefix('.') {
        Some(rest) => {
            let (digits, after) = take_digits(rest);
            if digits.is_empty() { return (0, s); }
            (digits_to_nanos(digits) as u32, after)
        }
        None => (0, s),
    }
}

/// Convert a fractional-second digit string to nanoseconds.
/// `"5"` → 500_000_000, `"123456789"` → 123_456_789, `"1234567890"`
/// truncates to nanos.
fn digits_to_nanos(digits: &str) -> i64 {
    let mut buf = String::with_capacity(9);
    for (i, c) in digits.chars().enumerate() {
        if i >= 9 { break; }
        buf.push(c);
    }
    while buf.len() < 9 { buf.push('0'); }
    buf.parse().unwrap_or(0)
}

/// `Z` | `+hh:mm` | `-hh:mm` | (nothing).  Returns the offset and
/// remainder string.
fn parse_optional_tz(s: &str) -> (Option<FixedOffset>, &str) {
    if let Some(rest) = s.strip_prefix('Z') {
        return (FixedOffset::east_opt(0), rest);
    }
    let sign = match s.as_bytes().first() {
        Some(b'+') => 1,
        Some(b'-') => -1,
        _ => return (None, s),
    };
    let rest = &s[1..];
    let Some((h, r)) = take_n_digits(rest, 2) else { return (None, s); };
    let Some(r) = expect_char(r, ':') else { return (None, s); };
    let Some((m, r)) = take_n_digits(r, 2) else { return (None, s); };
    let total = sign * ((h as i32) * 3600 + (m as i32) * 60);
    (FixedOffset::east_opt(total), r)
}

// ── EXSLT functions ──────────────────────────────────────────────

fn now_dt() -> Dt {
    // `std::time::SystemTime` → `DateTime<Utc>` without pulling in
    // `iana-time-zone` (the chrono `clock` feature does pull it in).
    let dur = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs  = dur.as_secs() as i64;
    let nanos = dur.subsec_nanos();
    let dt = DateTime::<Utc>::from_timestamp(secs, nanos)
        .unwrap_or_else(|| Utc.timestamp_opt(0, 0).unwrap());
    Dt {
        naive: dt.naive_utc(),
        offset: Some(FixedOffset::east_opt(0).unwrap()),
    }
}

fn arg_str<I: DocIndexLike>(args: &[Value], n: usize, idx: &I) -> Option<String> {
    args.get(n).map(|v| value_to_string(v, idx))
}

/// Many EXSLT functions accept an optional dateTime argument and
/// fall back to "now" if absent.  Returns the parsed value or None
/// if the supplied string was unparseable (caller emits "").
fn dt_arg_or_now<I: DocIndexLike>(args: &[Value], idx: &I) -> Option<Dt> {
    if args.is_empty() {
        return Some(now_dt());
    }
    parse_any(&arg_str(args, 0, idx)?)
}

pub fn dispatch<I: DocIndexLike>(
    name: &str, args: Vec<Value>, idx: &I,
) -> Option<Result<Value>> {
    let r: Result<Value> = match name {
        "date-time" => {
            if !args.is_empty() {
                return Some(Err(err("date:date-time takes no arguments")));
            }
            Ok(Value::String(format_xsd_datetime(&now_dt())))
        }
        "date" => Ok(extract_part(&args, idx, |dt| format_xsd_date(&dt))),
        "time" => Ok(extract_part(&args, idx, |dt| format_xsd_time(&dt))),

        "year"            => Ok(num_field(&args, idx, |dt| dt.naive.year() as f64)),
        "month-in-year"   => Ok(num_field(&args, idx, |dt| dt.naive.month() as f64)),
        "day-in-month"    => Ok(num_field(&args, idx, |dt| dt.naive.day() as f64)),
        "hour-in-day"     => Ok(num_field(&args, idx, |dt| dt.naive.hour() as f64)),
        "minute-in-hour"  => Ok(num_field(&args, idx, |dt| dt.naive.minute() as f64)),
        "second-in-minute"=> Ok(num_field(&args, idx, |dt| {
            // Include fractional seconds (EXSLT spec).
            dt.naive.second() as f64 + dt.naive.nanosecond() as f64 / 1e9
        })),

        // Days-of-week: EXSLT numbers Sunday=1..Saturday=7.
        "day-in-week" => Ok(num_field(&args, idx, |dt| {
            // chrono's Weekday::number_from_sunday() returns 1..7
            // with Sunday=1 — exactly EXSLT's numbering.
            dt.naive.weekday().number_from_sunday() as f64
        })),
        "day-name" => Ok(str_field(&args, idx, |dt| match dt.naive.weekday() {
            chrono::Weekday::Mon => "Monday",   chrono::Weekday::Tue => "Tuesday",
            chrono::Weekday::Wed => "Wednesday",chrono::Weekday::Thu => "Thursday",
            chrono::Weekday::Fri => "Friday",   chrono::Weekday::Sat => "Saturday",
            chrono::Weekday::Sun => "Sunday",
        })),
        "day-abbreviation" => Ok(str_field(&args, idx, |dt| match dt.naive.weekday() {
            chrono::Weekday::Mon => "Mon", chrono::Weekday::Tue => "Tue",
            chrono::Weekday::Wed => "Wed", chrono::Weekday::Thu => "Thu",
            chrono::Weekday::Fri => "Fri", chrono::Weekday::Sat => "Sat",
            chrono::Weekday::Sun => "Sun",
        })),
        "month-name" => Ok(str_field(&args, idx, |dt| MONTH_NAMES[(dt.naive.month() - 1) as usize])),
        "month-abbreviation" => Ok(str_field(&args, idx, |dt| MONTH_ABBR[(dt.naive.month() - 1) as usize])),

        // "Which occurrence in the month is this weekday?"  Day 1-7
        // is #1, 8-14 is #2, … 22-28 is #4, 29-31 is #5.
        "day-of-week-in-month" => Ok(num_field(&args, idx, |dt| {
            ((dt.naive.day() - 1) / 7 + 1) as f64
        })),

        // Julian day-of-year, 1-based.  Matches chrono's ordinal()
        // and ISO 8601 day numbering (Jan 1 → 1, Dec 31 → 365/366).
        "day-in-year"  => Ok(num_field(&args, idx, |dt| dt.naive.ordinal() as f64)),
        // ISO 8601 week number (1..53).  Weeks start on Monday and
        // belong to the year that contains the week's Thursday — so
        // early-Jan dates may report the previous year's last week.
        "week-in-year" => Ok(num_field(&args, idx, |dt| dt.naive.iso_week().week() as f64)),
        "leap-year"    => Ok(bool_field(&args, idx, |dt| {
            // Gregorian leap-year rule: divisible by 4, except
            // century years which must also be divisible by 400.
            let y = dt.naive.year();
            (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
        })),
        // EXSLT date:seconds — return seconds-since-Unix-epoch for a
        // date/dateTime, or total seconds for a duration.  No arg →
        // now.  Bad input → NaN, mirroring the other num_field
        // accessors (libexslt returns empty string which coerces to
        // NaN under XPath number()).
        "seconds" => Ok(seconds_fn(&args, idx)),

        "add" => Ok(add_fn(&args, idx)),
        "add-duration" => Ok(add_duration_fn(&args, idx)),
        "difference" => Ok(difference_fn(&args, idx)),
        "duration" => Ok(duration_from_seconds(&args, idx)),

        _ => return None,
    };
    Some(r)
}

const MONTH_NAMES: [&str; 12] = [
    "January", "February", "March", "April", "May", "June",
    "July", "August", "September", "October", "November", "December",
];
const MONTH_ABBR: [&str; 12] = [
    "Jan","Feb","Mar","Apr","May","Jun","Jul","Aug","Sep","Oct","Nov","Dec",
];

/// "If the argument is not present, returns the current xs:date /
/// xs:time"-style helpers.  Bad input → empty string, per spec.
fn extract_part<I: DocIndexLike>(
    args: &[Value], idx: &I, fmt: impl FnOnce(Dt) -> String,
) -> Value {
    Value::String(dt_arg_or_now(args, idx).map(fmt).unwrap_or_default())
}

fn num_field<I: DocIndexLike>(
    args: &[Value], idx: &I, f: impl FnOnce(Dt) -> f64,
) -> Value {
    Value::Number(Numeric::Double(dt_arg_or_now(args, idx).map(f).unwrap_or(f64::NAN)))
}

fn str_field<I: DocIndexLike>(
    args: &[Value], idx: &I, f: impl FnOnce(Dt) -> &'static str,
) -> Value {
    Value::String(dt_arg_or_now(args, idx).map(f).unwrap_or("").to_string())
}

fn bool_field<I: DocIndexLike>(
    args: &[Value], idx: &I, f: impl FnOnce(Dt) -> bool,
) -> Value {
    Value::Boolean(dt_arg_or_now(args, idx).map(f).unwrap_or(false))
}

/// `date:seconds(string?)` — seconds as a double.
///
/// * Date / dateTime → seconds since `1970-01-01T00:00:00Z` (negative
///   for pre-epoch values).
/// * Duration → total seconds (negative when the duration is signed
///   negative).
/// * No arg → seconds since epoch for the current dateTime.
/// * Anything else (unparseable) → NaN, mirroring the other
///   numeric accessors.
///
/// Duration-to-seconds approximation: per EXSLT spec, year and
/// month components in an `xs:duration` are *imprecise* — they
/// have no fixed length in seconds.  libexslt uses
/// `1 year = 365.2422 days` and `1 month = 30.4368 days`.  We
/// match those constants so the numbers agree on durations
/// containing year/month parts.
fn seconds_fn<I: DocIndexLike>(args: &[Value], idx: &I) -> Value {
    if args.is_empty() {
        return Value::Number(Numeric::Double(now_dt().to_utc().timestamp() as f64));
    }
    let s = value_to_string(&args[0], idx);
    if let Some(dt) = parse_any(&s) {
        let utc = dt.to_utc();
        return Value::Number(Numeric::Double(
            utc.timestamp() as f64 + utc.timestamp_subsec_nanos() as f64 / 1e9
        ));
    }
    if let Some(dur) = parse_xsd_duration(&s) {
        // Year+month components have no fixed length in seconds.
        // libexslt approximates with `1 month = 30.4368 days`
        // (matches `365.2422 / 12` — the mean tropical year over the
        // Gregorian cycle).  We match those constants so cross-
        // implementation results agree on year/month durations.
        const SECONDS_PER_DAY: f64 = 86400.0;
        const DAYS_PER_MONTH:  f64 = 30.4368;
        let total =
              dur.months  as f64 * DAYS_PER_MONTH * SECONDS_PER_DAY
            + dur.days    as f64 * SECONDS_PER_DAY
            + dur.seconds as f64
            + dur.nanos   as f64 / 1e9;
        return Value::Number(Numeric::Double(if dur.negative { -total } else { total }));
    }
    Value::Number(Numeric::Double(f64::NAN))
}

/// `date:add(date, duration)` — return the date offset by duration.
fn add_fn<I: DocIndexLike>(args: &[Value], idx: &I) -> Value {
    if args.len() != 2 { return Value::String(String::new()); }
    let Some(date) = parse_any(&value_to_string(&args[0], idx)) else {
        return Value::String(String::new());
    };
    let Some(dur)  = parse_xsd_duration(&value_to_string(&args[1], idx)) else {
        return Value::String(String::new());
    };
    let m = dur.months_part();
    // chrono::Months::new takes u32; sign handled separately.
    let mut nd = date.naive;
    if m != 0 {
        let months = chrono::Months::new(m.unsigned_abs() as u32);
        nd = if m > 0 {
            nd.checked_add_months(months)
        } else {
            nd.checked_sub_months(months)
        }
        .unwrap_or(nd);   // overflow → keep input date (libexslt fallback)
    }
    let new_dt = nd.checked_add_signed(dur.duration_part()).unwrap_or(nd);
    let result = Dt { naive: new_dt, offset: date.offset };
    // Re-emit in the input's lexical shape — heuristic: if the
    // input parses as xs:date (no T), emit xs:date; else xs:dateTime.
    let s = value_to_string(&args[0], idx);
    let out = if parse_xsd_date(&s).is_some() && !s.contains('T') {
        format_xsd_date(&result)
    } else {
        format_xsd_datetime(&result)
    };
    Value::String(out)
}

/// `date:add-duration(d1, d2)`.  XSD §E adding-duration-to-duration:
/// only valid when both have the same calendar/fixed split (i.e.
/// you can't sensibly add "1 month" to "30 days" without a ref date)
/// — EXSLT just adds component-wise and lets the user worry about it.
fn add_duration_fn<I: DocIndexLike>(args: &[Value], idx: &I) -> Value {
    if args.len() != 2 { return Value::String(String::new()); }
    let Some(a) = parse_xsd_duration(&value_to_string(&args[0], idx)) else {
        return Value::String(String::new());
    };
    let Some(b) = parse_xsd_duration(&value_to_string(&args[1], idx)) else {
        return Value::String(String::new());
    };
    let sa = if a.negative { -1i64 } else { 1 };
    let sb = if b.negative { -1i64 } else { 1 };
    let months   = sa * a.months   + sb * b.months;
    let days     = sa * a.days     + sb * b.days;
    let seconds  = sa * a.seconds  + sb * b.seconds;
    let nanos    = sa * a.nanos    + sb * b.nanos;
    let negative = months < 0 || (months == 0 && (days < 0 || (days == 0 && (seconds < 0 || nanos < 0))));
    let abs_months  = months.abs();
    let abs_days    = days.abs();
    let abs_seconds = seconds.abs();
    let abs_nanos   = nanos.abs();
    Value::String(format_xsd_duration(&XsdDuration {
        negative, months: abs_months, days: abs_days, seconds: abs_seconds, nanos: abs_nanos,
    }))
}

/// `date:difference(start, end)` — returns duration (end − start)
/// as xs:duration.  Cross-zone safe via UTC normalisation.
fn difference_fn<I: DocIndexLike>(args: &[Value], idx: &I) -> Value {
    if args.len() != 2 { return Value::String(String::new()); }
    let Some(a) = parse_any(&value_to_string(&args[0], idx)) else {
        return Value::String(String::new());
    };
    let Some(b) = parse_any(&value_to_string(&args[1], idx)) else {
        return Value::String(String::new());
    };
    let delta_secs = b.to_utc().signed_duration_since(a.to_utc()).num_seconds();
    let negative = delta_secs < 0;
    Value::String(format_xsd_duration(&XsdDuration {
        negative,
        months: 0,
        days:    delta_secs.unsigned_abs() as i64 / 86_400,
        seconds: delta_secs.unsigned_abs() as i64 % 86_400,
        nanos:   0,
    }))
}

/// `date:duration(seconds)` — turn a numeric second count into an
/// xs:duration string.  Negative is allowed; fractional is allowed.
fn duration_from_seconds<I: DocIndexLike>(args: &[Value], idx: &I) -> Value {
    if args.len() != 1 { return Value::String(String::new()); }
    let n = crate::xpath::eval::value_to_number(&args[0], idx);
    if !n.is_finite() { return Value::String(String::new()); }
    let neg  = n < 0.0;
    let abs  = n.abs();
    let secs = abs.trunc() as i64;
    let nanos = (abs.fract() * 1e9).round() as i64;
    Value::String(format_xsd_duration(&XsdDuration {
        negative: neg,
        months: 0,
        days:    secs / 86_400,
        seconds: secs % 86_400,
        nanos,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xpath::XPathContext;
    use crate::{parse_str, ParseOptions};

    fn tiny() -> sup_xml_tree::dom::Document {
        parse_str("<r/>", &ParseOptions::default()).unwrap()
    }
    fn s(v: &Value) -> String {
        if let Value::String(s) = v { s.clone() } else { panic!("expected string, got {v:?}") }
    }
    fn n(v: &Value) -> f64 {
        if let Value::Number(n) = v { n.as_f64() } else { panic!("expected number, got {v:?}") }
    }
    fn b(v: &Value) -> bool {
        if let Value::Boolean(b) = v { *b } else { panic!("expected boolean, got {v:?}") }
    }

    // ── parser round-trips ──────────────────────────────────────

    #[test]
    fn datetime_parse_basic() {
        let dt = parse_xsd_datetime("2024-03-15T14:30:45Z").unwrap();
        assert_eq!(dt.naive.year(), 2024);
        assert_eq!(dt.naive.month(), 3);
        assert_eq!(dt.naive.day(), 15);
        assert_eq!(dt.naive.hour(), 14);
        assert_eq!(dt.offset, Some(FixedOffset::east_opt(0).unwrap()));
    }

    #[test]
    fn datetime_parse_with_offset() {
        let dt = parse_xsd_datetime("2024-03-15T14:30:45+05:30").unwrap();
        assert_eq!(dt.offset.unwrap().local_minus_utc(), 5*3600 + 30*60);
    }

    #[test]
    fn datetime_parse_fractional_seconds() {
        let dt = parse_xsd_datetime("2024-03-15T14:30:45.5Z").unwrap();
        assert_eq!(dt.naive.nanosecond(), 500_000_000);
    }

    #[test]
    fn datetime_rejects_year_zero() {
        assert!(parse_xsd_datetime("0000-01-01T00:00:00Z").is_none());
    }

    #[test]
    fn date_parse_negative_year() {
        let dt = parse_xsd_date("-0044-03-15").unwrap();
        assert_eq!(dt.naive.year(), -44);
    }

    #[test]
    fn duration_parse_full() {
        let d = parse_xsd_duration("P1Y2M3DT4H5M6S").unwrap();
        assert_eq!(d.months, 14);
        assert_eq!(d.days,   3);
        assert_eq!(d.seconds, 4*3600 + 5*60 + 6);
    }

    #[test]
    fn duration_parse_negative_with_fraction() {
        let d = parse_xsd_duration("-PT1.5S").unwrap();
        assert!(d.negative);
        assert_eq!(d.seconds, 1);
        assert_eq!(d.nanos, 500_000_000);
    }

    #[test]
    fn duration_rejects_empty() {
        assert!(parse_xsd_duration("P").is_none());
        assert!(parse_xsd_duration("PT").is_none());
    }

    // ── dispatch ────────────────────────────────────────────────

    #[test]
    fn year_extracts_year_field() {
        let doc = tiny();
        let ctx = XPathContext::new(&doc);
        let v = dispatch("year",
            vec![Value::String("2024-03-15T00:00:00Z".into())],
            &ctx.index).unwrap().unwrap();
        assert_eq!(n(&v), 2024.0);
    }

    #[test]
    fn day_in_week_uses_sunday_one_convention() {
        let doc = tiny();
        let ctx = XPathContext::new(&doc);
        // 2024-03-17 is a Sunday → 1.
        let v = dispatch("day-in-week",
            vec![Value::String("2024-03-17".into())], &ctx.index).unwrap().unwrap();
        assert_eq!(n(&v), 1.0);
        // 2024-03-16 is a Saturday → 7.
        let v = dispatch("day-in-week",
            vec![Value::String("2024-03-16".into())], &ctx.index).unwrap().unwrap();
        assert_eq!(n(&v), 7.0);
    }

    #[test]
    fn day_name_returns_full_name() {
        let doc = tiny();
        let ctx = XPathContext::new(&doc);
        let v = dispatch("day-name",
            vec![Value::String("2024-03-15".into())], &ctx.index).unwrap().unwrap();
        assert_eq!(s(&v), "Friday");
    }

    #[test]
    fn month_name_returns_full_name() {
        let doc = tiny();
        let ctx = XPathContext::new(&doc);
        let v = dispatch("month-name",
            vec![Value::String("2024-03-15".into())], &ctx.index).unwrap().unwrap();
        assert_eq!(s(&v), "March");
    }

    // ── day-in-year / week-in-year / leap-year / seconds ───────

    #[test]
    fn day_in_year_jan_first_is_one() {
        let doc = tiny();
        let ctx = XPathContext::new(&doc);
        let v = dispatch("day-in-year",
            vec![Value::String("2024-01-01".into())], &ctx.index).unwrap().unwrap();
        assert_eq!(n(&v), 1.0);
    }

    #[test]
    fn day_in_year_leap_year_dec_31_is_366() {
        let doc = tiny();
        let ctx = XPathContext::new(&doc);
        let v = dispatch("day-in-year",
            vec![Value::String("2024-12-31".into())], &ctx.index).unwrap().unwrap();
        assert_eq!(n(&v), 366.0);
        let v = dispatch("day-in-year",
            vec![Value::String("2023-12-31".into())], &ctx.index).unwrap().unwrap();
        assert_eq!(n(&v), 365.0);
    }

    #[test]
    fn week_in_year_iso_8601_numbering() {
        let doc = tiny();
        let ctx = XPathContext::new(&doc);
        // 2024-01-01 is a Monday → week 1 of 2024.
        let v = dispatch("week-in-year",
            vec![Value::String("2024-01-01".into())], &ctx.index).unwrap().unwrap();
        assert_eq!(n(&v), 1.0);
        // 2023-01-01 is a Sunday → ISO week 52 of 2022.
        let v = dispatch("week-in-year",
            vec![Value::String("2023-01-01".into())], &ctx.index).unwrap().unwrap();
        assert_eq!(n(&v), 52.0);
    }

    #[test]
    fn leap_year_handles_century_exception() {
        let doc = tiny();
        let ctx = XPathContext::new(&doc);
        let v = dispatch("leap-year",
            vec![Value::String("2024-06-15".into())], &ctx.index).unwrap().unwrap();
        assert!(b(&v));
        let v = dispatch("leap-year",
            vec![Value::String("2023-06-15".into())], &ctx.index).unwrap().unwrap();
        assert!(!b(&v));
        // 1900 divisible by 100 but not 400 → not leap.
        let v = dispatch("leap-year",
            vec![Value::String("1900-06-15".into())], &ctx.index).unwrap().unwrap();
        assert!(!b(&v));
        // 2000 divisible by 400 → leap.
        let v = dispatch("leap-year",
            vec![Value::String("2000-06-15".into())], &ctx.index).unwrap().unwrap();
        assert!(b(&v));
    }

    #[test]
    fn seconds_from_datetime_is_unix_epoch_seconds() {
        let doc = tiny();
        let ctx = XPathContext::new(&doc);
        let v = dispatch("seconds",
            vec![Value::String("1970-01-01T00:00:00Z".into())], &ctx.index).unwrap().unwrap();
        assert_eq!(n(&v), 0.0);
        let v = dispatch("seconds",
            vec![Value::String("1970-01-01T00:01:30Z".into())], &ctx.index).unwrap().unwrap();
        assert_eq!(n(&v), 90.0);
    }

    #[test]
    fn seconds_from_date_treats_as_midnight_utc() {
        let doc = tiny();
        let ctx = XPathContext::new(&doc);
        let v = dispatch("seconds",
            vec![Value::String("1970-01-02".into())], &ctx.index).unwrap().unwrap();
        assert_eq!(n(&v), 86400.0);
    }

    #[test]
    fn seconds_from_duration_sums_components() {
        let doc = tiny();
        let ctx = XPathContext::new(&doc);
        let v = dispatch("seconds",
            vec![Value::String("PT1H30M".into())], &ctx.index).unwrap().unwrap();
        assert_eq!(n(&v), 5400.0);
        let v = dispatch("seconds",
            vec![Value::String("-PT10S".into())], &ctx.index).unwrap().unwrap();
        assert_eq!(n(&v), -10.0);
    }

    #[test]
    fn seconds_bad_input_is_nan() {
        let doc = tiny();
        let ctx = XPathContext::new(&doc);
        let v = dispatch("seconds",
            vec![Value::String("not a date".into())], &ctx.index).unwrap().unwrap();
        assert!(n(&v).is_nan());
    }

    // ── arithmetic ─────────────────────────────────────────────

    #[test]
    fn add_one_month_to_jan_31_clips_to_feb_29_in_leap_year() {
        let doc = tiny();
        let ctx = XPathContext::new(&doc);
        let v = dispatch("add",
            vec![Value::String("2024-01-31".into()), Value::String("P1M".into())],
            &ctx.index).unwrap().unwrap();
        assert_eq!(s(&v), "2024-02-29");
    }

    #[test]
    fn add_one_month_to_jan_31_clips_to_feb_28_in_non_leap_year() {
        let doc = tiny();
        let ctx = XPathContext::new(&doc);
        let v = dispatch("add",
            vec![Value::String("2023-01-31".into()), Value::String("P1M".into())],
            &ctx.index).unwrap().unwrap();
        assert_eq!(s(&v), "2023-02-28");
    }

    #[test]
    fn add_negative_duration_subtracts() {
        let doc = tiny();
        let ctx = XPathContext::new(&doc);
        let v = dispatch("add",
            vec![Value::String("2024-03-15".into()), Value::String("-P1D".into())],
            &ctx.index).unwrap().unwrap();
        assert_eq!(s(&v), "2024-03-14");
    }

    #[test]
    fn difference_returns_xsd_duration() {
        let doc = tiny();
        let ctx = XPathContext::new(&doc);
        let v = dispatch("difference",
            vec![
                Value::String("2024-01-01T00:00:00Z".into()),
                Value::String("2024-01-02T00:00:00Z".into()),
            ], &ctx.index).unwrap().unwrap();
        assert_eq!(s(&v), "P1D");
    }

    #[test]
    fn duration_from_seconds_formats_correctly() {
        let doc = tiny();
        let ctx = XPathContext::new(&doc);
        let v = dispatch("duration",
            vec![Value::Number(Numeric::Double(90061.5))], &ctx.index).unwrap().unwrap();
        // 90061.5s = 1 day + 1 hour + 1 minute + 1.5 second.
        assert_eq!(s(&v), "P1DT1H1M1.5S");
    }

    // ── defensive behaviour ────────────────────────────────────

    #[test]
    fn malformed_input_returns_empty_not_error() {
        let doc = tiny();
        let ctx = XPathContext::new(&doc);
        let v = dispatch("year",
            vec![Value::String("not-a-date".into())], &ctx.index).unwrap().unwrap();
        // Spec: bad input → NaN (for number-returning) / "" (for string-returning).
        assert!(n(&v).is_nan());
    }

    #[test]
    fn unknown_function_returns_none() {
        let doc = tiny();
        let ctx = XPathContext::new(&doc);
        assert!(dispatch("nonsense-name", vec![], &ctx.index).is_none());
    }

    // ── parse_xsd_time ──────────────────────────────────────────────

    #[test]
    fn parse_xsd_time_basic() {
        let dt = parse_xsd_time("14:30:45Z").unwrap();
        assert_eq!(dt.naive.hour(), 14);
        assert_eq!(dt.naive.minute(), 30);
        assert_eq!(dt.naive.second(), 45);
        // Date defaults to 1970-01-01.
        assert_eq!(dt.naive.year(), 1970);
        assert_eq!(dt.naive.month(), 1);
        assert_eq!(dt.naive.day(), 1);
    }

    #[test]
    fn parse_xsd_time_with_fractional_and_offset() {
        let dt = parse_xsd_time("14:30:45.25+05:30").unwrap();
        assert_eq!(dt.naive.nanosecond(), 250_000_000);
        assert_eq!(dt.offset.unwrap().local_minus_utc(), 5*3600 + 30*60);
    }

    #[test]
    fn parse_xsd_time_no_offset() {
        let dt = parse_xsd_time("14:30:45").unwrap();
        assert!(dt.offset.is_none());
    }

    #[test]
    fn parse_xsd_time_rejects_garbage() {
        assert!(parse_xsd_time("14:30").is_none());
        assert!(parse_xsd_time("14:30:45Zextra").is_none());
        assert!(parse_xsd_time("99:30:45").is_none());
    }

    // ── duration parser error paths ─────────────────────────────────

    #[test]
    fn parse_xsd_duration_rejects_bad_designator_in_date_part() {
        // 'P' part allows only Y/M/D — any other letter → None.
        assert!(parse_xsd_duration("P1X").is_none());
    }

    #[test]
    fn parse_xsd_duration_rejects_bad_designator_in_time_part() {
        // 'T' part allows H/M/S — anything else → None.
        assert!(parse_xsd_duration("PT1X").is_none());
    }

    // ── formatters ──────────────────────────────────────────────────

    #[test]
    fn format_xsd_datetime_round_trip() {
        // No fractional seconds, Z offset.
        let dt = parse_xsd_datetime("2024-03-15T14:30:45Z").unwrap();
        assert_eq!(format_xsd_datetime(&dt), "2024-03-15T14:30:45Z");
    }

    #[test]
    fn format_xsd_datetime_with_fraction_and_offset() {
        let dt = parse_xsd_datetime("2024-03-15T14:30:45.5+05:30").unwrap();
        let out = format_xsd_datetime(&dt);
        assert!(out.starts_with("2024-03-15T14:30:45.5"));
        assert!(out.ends_with("+05:30"));
    }

    #[test]
    fn format_xsd_datetime_no_offset() {
        let dt = parse_xsd_datetime("2024-03-15T14:30:45").unwrap();
        // No offset → no Z/sign at the end.
        let out = format_xsd_datetime(&dt);
        assert_eq!(out, "2024-03-15T14:30:45");
    }

    #[test]
    fn format_xsd_time_via_dispatch() {
        let doc = tiny();
        let ctx = XPathContext::new(&doc);
        let v = dispatch("time",
            vec![Value::String("2024-03-15T14:30:45.5Z".into())],
            &ctx.index).unwrap().unwrap();
        let out = s(&v);
        assert!(out.starts_with("14:30:45"));
        assert!(out.contains('Z'));
    }

    #[test]
    fn format_offset_zero_is_z() {
        let off = FixedOffset::east_opt(0).unwrap();
        assert_eq!(format_offset(off), "Z");
    }

    #[test]
    fn format_offset_negative() {
        let off = FixedOffset::west_opt(5 * 3600).unwrap();
        assert_eq!(format_offset(off), "-05:00");
    }

    #[test]
    fn format_offset_positive_non_aligned() {
        let off = FixedOffset::east_opt(5 * 3600 + 30 * 60).unwrap();
        assert_eq!(format_offset(off), "+05:30");
    }

    #[test]
    fn format_xsd_duration_zero_emits_pt0s() {
        let d = XsdDuration {
            negative: false, months: 0, days: 0, seconds: 0, nanos: 0,
        };
        let out = format_xsd_duration(&d);
        assert_eq!(out, "PT0S");
    }

    #[test]
    fn format_xsd_duration_seconds_only() {
        let d = XsdDuration {
            negative: false, months: 0, days: 0, seconds: 45, nanos: 0,
        };
        assert_eq!(format_xsd_duration(&d), "PT45S");
    }

    // ── TZ parser: negative offset ──────────────────────────────────

    #[test]
    fn parse_optional_tz_negative_offset() {
        let dt = parse_xsd_datetime("2024-03-15T14:30:45-08:00").unwrap();
        assert_eq!(dt.offset.unwrap().local_minus_utc(), -8 * 3600);
    }

    // ── now_dt / date-time dispatch ─────────────────────────────────

    #[test]
    fn date_time_dispatch_with_no_args_returns_current_time() {
        let doc = tiny();
        let ctx = XPathContext::new(&doc);
        let v = dispatch("date-time", vec![], &ctx.index).unwrap().unwrap();
        let out = s(&v);
        // Smoke test: must parse back as a valid xs:dateTime.
        assert!(parse_xsd_datetime(&out).is_some(), "got {out:?}");
    }

    #[test]
    fn date_time_dispatch_rejects_args() {
        let doc = tiny();
        let ctx = XPathContext::new(&doc);
        let r = dispatch("date-time",
            vec![Value::String("ignored".into())], &ctx.index).unwrap();
        assert!(r.is_err());
    }

    // ── num_field/str_field without args use now_dt ─────────────────

    #[test]
    fn year_with_no_args_returns_current_year() {
        let doc = tiny();
        let ctx = XPathContext::new(&doc);
        let v = dispatch("year", vec![], &ctx.index).unwrap().unwrap();
        let y = n(&v);
        // Should be a plausible year, not NaN.
        assert!(y >= 2020.0 && y <= 3000.0, "got {y}");
    }

    // ── second-in-minute includes fractional seconds ────────────────

    #[test]
    fn second_in_minute_includes_fraction() {
        let doc = tiny();
        let ctx = XPathContext::new(&doc);
        let v = dispatch("second-in-minute",
            vec![Value::String("2024-03-15T14:30:45.25Z".into())],
            &ctx.index).unwrap().unwrap();
        let got = n(&v);
        assert!((got - 45.25).abs() < 1e-9, "got {got}");
    }

    // ── all weekday names + abbreviations ───────────────────────────

    #[test]
    fn all_weekday_names() {
        let doc = tiny();
        let ctx = XPathContext::new(&doc);
        // 2024-03-11 = Monday → 2024-03-17 = Sunday.
        let cases = [
            ("2024-03-11", "Monday",    "Mon"),
            ("2024-03-12", "Tuesday",   "Tue"),
            ("2024-03-13", "Wednesday", "Wed"),
            ("2024-03-14", "Thursday",  "Thu"),
            ("2024-03-15", "Friday",    "Fri"),
            ("2024-03-16", "Saturday",  "Sat"),
            ("2024-03-17", "Sunday",    "Sun"),
        ];
        for (date, full, abbr) in cases {
            let v = dispatch("day-name",
                vec![Value::String(date.into())], &ctx.index).unwrap().unwrap();
            assert_eq!(s(&v), full, "day-name for {date}");
            let v = dispatch("day-abbreviation",
                vec![Value::String(date.into())], &ctx.index).unwrap().unwrap();
            assert_eq!(s(&v), abbr, "day-abbreviation for {date}");
        }
    }

    #[test]
    fn all_month_names() {
        let doc = tiny();
        let ctx = XPathContext::new(&doc);
        let names = ["January","February","March","April","May","June",
                     "July","August","September","October","November","December"];
        let abbrs = ["Jan","Feb","Mar","Apr","May","Jun","Jul","Aug","Sep","Oct","Nov","Dec"];
        for (i, (name, abbr)) in names.iter().zip(abbrs.iter()).enumerate() {
            let date = format!("2024-{:02}-15", i + 1);
            let v = dispatch("month-name",
                vec![Value::String(date.clone())], &ctx.index).unwrap().unwrap();
            assert_eq!(s(&v), *name, "month {} ({date})", i + 1);
            let v = dispatch("month-abbreviation",
                vec![Value::String(date)], &ctx.index).unwrap().unwrap();
            assert_eq!(s(&v), *abbr);
        }
    }

    // ── day-of-week-in-month ────────────────────────────────────────

    #[test]
    fn day_of_week_in_month_counts_weekday_occurrences() {
        let doc = tiny();
        let ctx = XPathContext::new(&doc);
        let cases = [
            ("2024-03-01", 1.0),    // day 1
            ("2024-03-07", 1.0),    // day 7
            ("2024-03-08", 2.0),    // day 8
            ("2024-03-15", 3.0),    // day 15
            ("2024-03-22", 4.0),    // day 22
            ("2024-03-29", 5.0),    // day 29
        ];
        for (date, expected) in cases {
            let v = dispatch("day-of-week-in-month",
                vec![Value::String(date.into())], &ctx.index).unwrap().unwrap();
            assert_eq!(n(&v), expected, "{date}");
        }
    }

    // ── add-duration ────────────────────────────────────────────────

    #[test]
    fn add_duration_combines_two_positive_durations() {
        let doc = tiny();
        let ctx = XPathContext::new(&doc);
        let v = dispatch("add-duration",
            vec![Value::String("P1Y".into()), Value::String("P6M".into())],
            &ctx.index).unwrap().unwrap();
        assert_eq!(s(&v), "P1Y6M");
    }

    #[test]
    fn add_duration_with_negative_yields_partial_cancellation() {
        let doc = tiny();
        let ctx = XPathContext::new(&doc);
        // P1Y + (-P6M) = P6M
        let v = dispatch("add-duration",
            vec![Value::String("P1Y".into()), Value::String("-P6M".into())],
            &ctx.index).unwrap().unwrap();
        assert_eq!(s(&v), "P6M");
    }

    #[test]
    fn add_duration_rejects_non_duration_args() {
        let doc = tiny();
        let ctx = XPathContext::new(&doc);
        let v = dispatch("add-duration",
            vec![Value::String("not-a-duration".into()), Value::String("P1M".into())],
            &ctx.index).unwrap().unwrap();
        assert_eq!(s(&v), "");
    }

    #[test]
    fn add_duration_wrong_argc_returns_empty() {
        let doc = tiny();
        let ctx = XPathContext::new(&doc);
        let v = dispatch("add-duration",
            vec![Value::String("P1Y".into())], &ctx.index).unwrap().unwrap();
        assert_eq!(s(&v), "");
    }

    // ── add edge cases ──────────────────────────────────────────────

    #[test]
    fn add_emits_datetime_when_input_has_time_part() {
        let doc = tiny();
        let ctx = XPathContext::new(&doc);
        let v = dispatch("add",
            vec![Value::String("2024-03-15T12:00:00Z".into()),
                 Value::String("P1D".into())],
            &ctx.index).unwrap().unwrap();
        let out = s(&v);
        assert!(out.contains('T'), "got {out}");
    }

    #[test]
    fn add_wrong_argc_returns_empty() {
        let doc = tiny();
        let ctx = XPathContext::new(&doc);
        let v = dispatch("add",
            vec![Value::String("2024-01-01".into())], &ctx.index).unwrap().unwrap();
        assert_eq!(s(&v), "");
    }

    #[test]
    fn add_with_malformed_date_returns_empty() {
        let doc = tiny();
        let ctx = XPathContext::new(&doc);
        let v = dispatch("add",
            vec![Value::String("not-a-date".into()),
                 Value::String("P1D".into())],
            &ctx.index).unwrap().unwrap();
        assert_eq!(s(&v), "");
    }

    #[test]
    fn add_with_malformed_duration_returns_empty() {
        let doc = tiny();
        let ctx = XPathContext::new(&doc);
        let v = dispatch("add",
            vec![Value::String("2024-01-01".into()),
                 Value::String("not-a-duration".into())],
            &ctx.index).unwrap().unwrap();
        assert_eq!(s(&v), "");
    }

    // ── difference edge cases ───────────────────────────────────────

    #[test]
    fn difference_negative_when_end_before_start() {
        let doc = tiny();
        let ctx = XPathContext::new(&doc);
        let v = dispatch("difference",
            vec![Value::String("2024-01-02T00:00:00Z".into()),
                 Value::String("2024-01-01T00:00:00Z".into())],
            &ctx.index).unwrap().unwrap();
        assert!(s(&v).starts_with('-'));
    }

    #[test]
    fn difference_wrong_argc_returns_empty() {
        let doc = tiny();
        let ctx = XPathContext::new(&doc);
        let v = dispatch("difference",
            vec![Value::String("2024-01-01".into())], &ctx.index).unwrap().unwrap();
        assert_eq!(s(&v), "");
    }

    #[test]
    fn difference_malformed_args_return_empty() {
        let doc = tiny();
        let ctx = XPathContext::new(&doc);
        let v = dispatch("difference",
            vec![Value::String("oops".into()),
                 Value::String("2024-01-01T00:00:00Z".into())],
            &ctx.index).unwrap().unwrap();
        assert_eq!(s(&v), "");
        let v = dispatch("difference",
            vec![Value::String("2024-01-01T00:00:00Z".into()),
                 Value::String("oops".into())],
            &ctx.index).unwrap().unwrap();
        assert_eq!(s(&v), "");
    }

    // ── duration() edge cases ───────────────────────────────────────

    #[test]
    fn duration_negative_seconds() {
        let doc = tiny();
        let ctx = XPathContext::new(&doc);
        let v = dispatch("duration",
            vec![Value::Number(Numeric::Double(-90.0))], &ctx.index).unwrap().unwrap();
        assert!(s(&v).starts_with('-'));
    }

    #[test]
    fn duration_wrong_argc_returns_empty() {
        let doc = tiny();
        let ctx = XPathContext::new(&doc);
        let v = dispatch("duration", vec![], &ctx.index).unwrap().unwrap();
        assert_eq!(s(&v), "");
    }

    #[test]
    fn duration_non_finite_returns_empty() {
        let doc = tiny();
        let ctx = XPathContext::new(&doc);
        let v = dispatch("duration",
            vec![Value::Number(Numeric::Double(f64::NAN))], &ctx.index).unwrap().unwrap();
        assert_eq!(s(&v), "");
        let v = dispatch("duration",
            vec![Value::Number(Numeric::Double(f64::INFINITY))], &ctx.index).unwrap().unwrap();
        assert_eq!(s(&v), "");
    }

    // ── month-in-year, day-in-month, hour-in-day, minute-in-hour ────

    #[test]
    fn extract_individual_fields() {
        let doc = tiny();
        let ctx = XPathContext::new(&doc);
        let dt = "2024-03-15T14:30:45Z";
        assert_eq!(n(&dispatch("month-in-year",
            vec![Value::String(dt.into())], &ctx.index).unwrap().unwrap()), 3.0);
        assert_eq!(n(&dispatch("day-in-month",
            vec![Value::String(dt.into())], &ctx.index).unwrap().unwrap()), 15.0);
        assert_eq!(n(&dispatch("hour-in-day",
            vec![Value::String(dt.into())], &ctx.index).unwrap().unwrap()), 14.0);
        assert_eq!(n(&dispatch("minute-in-hour",
            vec![Value::String(dt.into())], &ctx.index).unwrap().unwrap()), 30.0);
    }

    // ── date dispatch from a dateTime input ─────────────────────────

    #[test]
    fn date_dispatch_extracts_date_part_from_datetime() {
        let doc = tiny();
        let ctx = XPathContext::new(&doc);
        let v = dispatch("date",
            vec![Value::String("2024-03-15T14:30:45Z".into())],
            &ctx.index).unwrap().unwrap();
        assert!(s(&v).starts_with("2024-03-15"));
    }
}
