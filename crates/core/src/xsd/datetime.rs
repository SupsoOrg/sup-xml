//! XSD date/time/duration parsers.
//!
//! XSD §3.2.6–3.2.13 — eight calendar types plus `xs:duration`.  All use
//! an ISO-8601 *subset*; XSD diverges from full ISO-8601 in a few ways
//! (no week dates, year zero forbidden, leap-second support).
//!
//! Timezone:
//! * Optional on every type.
//! * `Z` means UTC (offset 0).
//! * `+HH:MM` / `-HH:MM` ranges from `-14:00` to `+14:00`.
//!
//! Comparison of two values without timezones is *partial* — they're
//! treated as distinct points in the implicit local timezone and compare
//! equal only on exact field match.  Comparisons with at least one
//! timezone-bearing operand normalise to UTC.

use std::cmp::Ordering;

use super::types::{TypeError, Value};

// ── value-space structs ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct XsdDateTime {
    /// Year — can be negative, can exceed 4 digits.  Year zero is illegal
    /// per XSD §3.2.7.
    pub year:   i32,
    pub month:  u8,
    pub day:    u8,
    pub hour:   u8,
    pub minute: u8,
    pub second: u8,
    /// Fractional seconds in nanoseconds (0..1_000_000_000).
    pub nanos:  u32,
    /// Offset from UTC in minutes (`-840..=840`).  `None` = no timezone.
    pub tz_min: Option<i16>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct XsdDate {
    pub year:   i32,
    pub month:  u8,
    pub day:    u8,
    pub tz_min: Option<i16>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct XsdTime {
    pub hour:   u8,
    pub minute: u8,
    pub second: u8,
    pub nanos:  u32,
    pub tz_min: Option<i16>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct XsdGYearMonth { pub year: i32, pub month: u8, pub tz_min: Option<i16> }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct XsdGYear { pub year: i32, pub tz_min: Option<i16> }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct XsdGMonthDay { pub month: u8, pub day: u8, pub tz_min: Option<i16> }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct XsdGDay { pub day: u8, pub tz_min: Option<i16> }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct XsdGMonth { pub month: u8, pub tz_min: Option<i16> }

/// `xs:duration` — split into a months part (years×12 + months) and a
/// seconds part (days×86400 + h×3600 + m×60 + s).  This split is forced
/// by the spec: months cannot be reduced to seconds without a reference
/// date.  Two durations compare equal only when both parts match.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct XsdDuration {
    pub months:  i64,
    /// Whole seconds — combined with `nanos` for sub-second precision.
    pub seconds: i64,
    pub nanos:   u32,
}

// ── tiny lexer ───────────────────────────────────────────────────────────────

struct Cur<'a> { s: &'a [u8], i: usize }

impl<'a> Cur<'a> {
    fn new(s: &'a str) -> Self { Self { s: s.as_bytes(), i: 0 } }
    fn done(&self) -> bool     { self.i >= self.s.len() }
    fn peek(&self) -> Option<u8> { self.s.get(self.i).copied() }
    fn bump(&mut self) -> Option<u8> {
        let b = self.peek()?; self.i += 1; Some(b)
    }
    fn expect(&mut self, b: u8) -> Result<(), TypeError> {
        if self.bump() == Some(b) {
            Ok(())
        } else {
            Err(TypeError::type_mismatch(format!("expected {:?}", b as char)))
        }
    }
    /// Read at least `min` decimal digits, return their value as i64.
    fn read_digits(&mut self, min: usize, max: usize) -> Result<i64, TypeError> {
        let start = self.i;
        while self.i < self.s.len()
            && self.s[self.i].is_ascii_digit()
            && self.i - start < max
        {
            self.i += 1;
        }
        let n = self.i - start;
        if n < min {
            return Err(TypeError::type_mismatch(
                format!("expected at least {min} digits, got {n}")
            ));
        }
        std::str::from_utf8(&self.s[start..self.i])
            .unwrap()
            .parse::<i64>()
            .map_err(|e| TypeError::type_mismatch(format!("digit parse: {e}")))
    }
    fn at_end(&self) -> bool { self.done() }
}

// ── component parsers (shared building blocks) ───────────────────────────────

fn read_year(c: &mut Cur) -> Result<i32, TypeError> {
    let neg = c.peek() == Some(b'-');
    if neg { c.bump(); }
    // At least 4 digits, can be more (year 12345 is legal).
    let start = c.i;
    while c.i < c.s.len() && c.s[c.i].is_ascii_digit() {
        c.i += 1;
    }
    let n = c.i - start;
    if n < 4 {
        return Err(TypeError::type_mismatch(
            format!("year requires at least 4 digits, got {n}")
        ));
    }
    // Reject leading zero on a year longer than 4 digits.
    if n > 4 && c.s[start] == b'0' {
        return Err(TypeError::type_mismatch(
            "extended year cannot start with 0"
        ));
    }
    let body: i64 = std::str::from_utf8(&c.s[start..c.i]).unwrap().parse().unwrap();
    if body == 0 {
        return Err(TypeError::type_mismatch("year zero is not allowed in XSD"));
    }
    let signed = if neg { -body } else { body };
    if signed < i32::MIN as i64 || signed > i32::MAX as i64 {
        return Err(TypeError::type_mismatch("year out of i32 range"));
    }
    Ok(signed as i32)
}

fn read_month(c: &mut Cur) -> Result<u8, TypeError> {
    let n = c.read_digits(2, 2)?;
    if !(1..=12).contains(&n) {
        return Err(TypeError::type_mismatch(format!("month {n} out of range")));
    }
    Ok(n as u8)
}

fn read_day(c: &mut Cur) -> Result<u8, TypeError> {
    let n = c.read_digits(2, 2)?;
    if !(1..=31).contains(&n) {
        return Err(TypeError::type_mismatch(format!("day {n} out of range")));
    }
    Ok(n as u8)
}

fn read_hh(c: &mut Cur) -> Result<u8, TypeError> {
    let n = c.read_digits(2, 2)?;
    if !(0..=24).contains(&n) {
        return Err(TypeError::type_mismatch(format!("hour {n} out of range")));
    }
    Ok(n as u8)
}

fn read_mm(c: &mut Cur) -> Result<u8, TypeError> {
    let n = c.read_digits(2, 2)?;
    if !(0..=59).contains(&n) {
        return Err(TypeError::type_mismatch(format!("minute {n} out of range")));
    }
    Ok(n as u8)
}

fn read_ss_with_nanos(c: &mut Cur) -> Result<(u8, u32), TypeError> {
    let s = c.read_digits(2, 2)?;
    if !(0..=60).contains(&s) {
        return Err(TypeError::type_mismatch(format!("second {s} out of range")));
    }
    let nanos = if c.peek() == Some(b'.') {
        c.bump();
        let start = c.i;
        while c.i < c.s.len() && c.s[c.i].is_ascii_digit() {
            c.i += 1;
        }
        let n = c.i - start;
        if n == 0 {
            return Err(TypeError::type_mismatch("expected fractional digits after '.'"));
        }
        // Convert up to 9 digits to nanoseconds; truncate beyond.
        let take = n.min(9);
        let bytes = &c.s[start..start + take];
        let val: u32 = std::str::from_utf8(bytes).unwrap().parse().unwrap();
        // Pad/scale to nanoseconds.
        let scale = 10u32.pow(9 - take as u32);
        val * scale
    } else { 0 };
    Ok((s as u8, nanos))
}

/// Parse the optional timezone suffix.  Returns `None` if EOF, the
/// offset in minutes otherwise.  Errors on partial/invalid suffix.
fn read_tz(c: &mut Cur) -> Result<Option<i16>, TypeError> {
    if c.done() { return Ok(None); }
    match c.bump().unwrap() {
        b'Z' => Ok(Some(0)),
        sign @ (b'+' | b'-') => {
            let h = c.read_digits(2, 2)?;
            c.expect(b':')?;
            let m = c.read_digits(2, 2)?;
            if h > 14 || m > 59 || (h == 14 && m != 0) {
                return Err(TypeError::type_mismatch(
                    format!("timezone {sign:?}{h:02}:{m:02} out of range")
                ));
            }
            let total = (h as i16) * 60 + (m as i16);
            Ok(Some(if sign == b'-' { -total } else { total }))
        }
        other => Err(TypeError::type_mismatch(
            format!("expected timezone (Z or +/-HH:MM), got {:?}", other as char)
        )),
    }
}

fn finish(c: &mut Cur) -> Result<(), TypeError> {
    if c.at_end() { Ok(()) }
    else { Err(TypeError::type_mismatch(format!(
        "trailing junk at offset {}", c.i
    ))) }
}

fn validate_day_in_month(year: i32, month: u8, day: u8) -> Result<(), TypeError> {
    let max = days_in_month(year, month);
    if day > max {
        return Err(TypeError::type_mismatch(
            format!("day {day} out of range for {year}-{month:02}")
        ));
    }
    Ok(())
}

fn days_in_month(year: i32, month: u8) -> u8 {
    match month {
        1|3|5|7|8|10|12 => 31,
        4|6|9|11 => 30,
        2 => if is_leap_year(year) { 29 } else { 28 },
        _ => 0,
    }
}

fn is_leap_year(y: i32) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

// ── public per-type parsers ──────────────────────────────────────────────────

pub fn parse_date_time(s: &str) -> Result<Value, TypeError> {
    let mut c = Cur::new(s);
    let year   = read_year(&mut c)?;
    c.expect(b'-')?;
    let month  = read_month(&mut c)?;
    c.expect(b'-')?;
    let day    = read_day(&mut c)?;
    validate_day_in_month(year, month, day)?;
    c.expect(b'T')?;
    let hour   = read_hh(&mut c)?;
    c.expect(b':')?;
    let minute = read_mm(&mut c)?;
    c.expect(b':')?;
    let (second, nanos) = read_ss_with_nanos(&mut c)?;
    if hour == 24 && (minute != 0 || second != 0 || nanos != 0) {
        return Err(TypeError::type_mismatch("24:xx:xx invalid (only 24:00:00 allowed)"));
    }
    let tz_min = read_tz(&mut c)?;
    finish(&mut c)?;
    Ok(Value::DateTime(XsdDateTime { year, month, day, hour, minute, second, nanos, tz_min }))
}

pub fn parse_date(s: &str) -> Result<Value, TypeError> {
    let mut c = Cur::new(s);
    let year  = read_year(&mut c)?;
    c.expect(b'-')?;
    let month = read_month(&mut c)?;
    c.expect(b'-')?;
    let day   = read_day(&mut c)?;
    validate_day_in_month(year, month, day)?;
    let tz_min = read_tz(&mut c)?;
    finish(&mut c)?;
    Ok(Value::Date(XsdDate { year, month, day, tz_min }))
}

pub fn parse_time(s: &str) -> Result<Value, TypeError> {
    let mut c = Cur::new(s);
    let hour   = read_hh(&mut c)?;
    c.expect(b':')?;
    let minute = read_mm(&mut c)?;
    c.expect(b':')?;
    let (second, nanos) = read_ss_with_nanos(&mut c)?;
    if hour == 24 && (minute != 0 || second != 0 || nanos != 0) {
        return Err(TypeError::type_mismatch("24:xx:xx invalid"));
    }
    let tz_min = read_tz(&mut c)?;
    finish(&mut c)?;
    Ok(Value::Time(XsdTime { hour, minute, second, nanos, tz_min }))
}

pub fn parse_g_year_month(s: &str) -> Result<Value, TypeError> {
    let mut c = Cur::new(s);
    let year  = read_year(&mut c)?;
    c.expect(b'-')?;
    let month = read_month(&mut c)?;
    let tz_min = read_tz(&mut c)?;
    finish(&mut c)?;
    Ok(Value::GYearMonth(XsdGYearMonth { year, month, tz_min }))
}

pub fn parse_g_year(s: &str) -> Result<Value, TypeError> {
    let mut c = Cur::new(s);
    let year  = read_year(&mut c)?;
    let tz_min = read_tz(&mut c)?;
    finish(&mut c)?;
    Ok(Value::GYear(XsdGYear { year, tz_min }))
}

pub fn parse_g_month_day(s: &str) -> Result<Value, TypeError> {
    let mut c = Cur::new(s);
    c.expect(b'-')?; c.expect(b'-')?;
    let month = read_month(&mut c)?;
    c.expect(b'-')?;
    let day   = read_day(&mut c)?;
    if day > days_in_month(2000 /* ignore-leap */, month) && !(month == 2 && day == 29) {
        return Err(TypeError::type_mismatch(
            format!("day {day} out of range for month {month:02}")
        ));
    }
    let tz_min = read_tz(&mut c)?;
    finish(&mut c)?;
    Ok(Value::GMonthDay(XsdGMonthDay { month, day, tz_min }))
}

pub fn parse_g_day(s: &str) -> Result<Value, TypeError> {
    let mut c = Cur::new(s);
    c.expect(b'-')?; c.expect(b'-')?; c.expect(b'-')?;
    let day = read_day(&mut c)?;
    let tz_min = read_tz(&mut c)?;
    finish(&mut c)?;
    Ok(Value::GDay(XsdGDay { day, tz_min }))
}

pub fn parse_g_month(s: &str) -> Result<Value, TypeError> {
    let mut c = Cur::new(s);
    c.expect(b'-')?; c.expect(b'-')?;
    let month = read_month(&mut c)?;
    // XSD 1.0 originally specified `--MM--`; XML Schema Errata
    // corrected the form to `--MM` while leaving the legacy form
    // in the wild. Accept the trailing `--` when it appears with
    // no time-zone designator in between.
    if c.peek() == Some(b'-') && c.s.get(c.i + 1).copied() == Some(b'-') {
        c.bump(); c.bump();
    }
    let tz_min = read_tz(&mut c)?;
    finish(&mut c)?;
    Ok(Value::GMonth(XsdGMonth { month, tz_min }))
}

// ── duration ─────────────────────────────────────────────────────────────────

pub fn parse_duration(s: &str) -> Result<Value, TypeError> {
    let mut c = Cur::new(s);
    let neg = c.peek() == Some(b'-');
    if neg { c.bump(); }
    c.expect(b'P')?;

    let mut years   = 0i64;
    let mut months  = 0i64;
    let mut days    = 0i64;
    let mut hours   = 0i64;
    let mut mins    = 0i64;
    let mut secs    = 0i64;
    let mut nanos   = 0u32;

    let mut saw_any_date = false;
    let mut saw_any_time = false;

    // Date part.
    while let Some(b) = c.peek() {
        if b == b'T' { break; }
        let n = c.read_digits(1, 18)?;
        match c.bump() {
            Some(b'Y') => { years   = n; saw_any_date = true; }
            Some(b'M') => { months  = n; saw_any_date = true; }
            Some(b'D') => { days    = n; saw_any_date = true; }
            Some(b'T') | None => return Err(TypeError::type_mismatch(
                "duration date part missing designator (Y/M/D)"
            )),
            Some(other) => return Err(TypeError::type_mismatch(
                format!("unexpected {:?} in duration date part", other as char)
            )),
        }
    }

    // Time part.
    if c.peek() == Some(b'T') {
        c.bump();
        if c.done() {
            return Err(TypeError::type_mismatch(
                "duration T must be followed by at least one time component"
            ));
        }
        while !c.done() {
            // Allow fractional seconds: read digits, then optionally `.frac`,
            // then designator.
            let n = c.read_digits(1, 18)?;
            // Look for a possible fractional part — only valid for seconds.
            let frac_nanos = if c.peek() == Some(b'.') {
                c.bump();
                let start = c.i;
                while c.i < c.s.len() && c.s[c.i].is_ascii_digit() { c.i += 1; }
                let take = (c.i - start).min(9);
                // XSD §3.2.6: the decimal point in a duration must be
                // followed by at least one digit.  `PT1.S` is a
                // malformed value — return a TypeError instead of
                // panicking inside `parse::<u32>()` on the empty
                // slice.
                if take == 0 {
                    return Err(TypeError::type_mismatch(
                        "duration: '.' must be followed by at least one fraction digit",
                    ));
                }
                let bytes = &c.s[start..start + take];
                // SAFETY of the two unwraps: `bytes` is a slice of
                // characters we just confirmed are ASCII digits (so
                // valid UTF-8), and at most 9 digits parses to at
                // most 999_999_999 which fits in u32.
                let val: u32 = std::str::from_utf8(bytes).unwrap().parse().unwrap();
                let scale = 10u32.pow(9 - take as u32);
                val * scale
            } else { 0 };
            match c.bump() {
                Some(b'H') => { hours = n; saw_any_time = true; }
                Some(b'M') => { mins  = n; saw_any_time = true; }
                Some(b'S') => { secs  = n; nanos = frac_nanos; saw_any_time = true; }
                Some(other) => return Err(TypeError::type_mismatch(
                    format!("unexpected {:?} in duration time part", other as char)
                )),
                None => return Err(TypeError::type_mismatch(
                    "duration time component missing designator"
                )),
            }
        }
    }

    if !saw_any_date && !saw_any_time {
        return Err(TypeError::type_mismatch("empty duration"));
    }

    let total_months  = years.saturating_mul(12).saturating_add(months);
    let total_seconds = days.saturating_mul(86400)
        .saturating_add(hours.saturating_mul(3600))
        .saturating_add(mins.saturating_mul(60))
        .saturating_add(secs);

    let (m, s, ns) = if neg {
        (-total_months, -total_seconds, nanos)
    } else {
        (total_months, total_seconds, nanos)
    };

    Ok(Value::Duration(XsdDuration { months: m, seconds: s, nanos: ns }))
}

// ── ordering helpers (used by order-facet checks for date/time types) ────────

impl XsdDateTime {
    /// Convert to a UTC instant in (year, day-of-year-style) units.
    /// Returns the number of seconds since 0001-01-01T00:00:00 UTC, after
    /// applying the timezone offset.  Fractional seconds in nanos.
    pub fn to_utc_seconds(&self) -> Option<(i128, u32)> {
        let tz = self.tz_min.unwrap_or(0);
        let base = days_from_civil(self.year, self.month, self.day);
        let mut seconds = (base as i128) * 86400
            + (self.hour as i128) * 3600
            + (self.minute as i128) * 60
            + self.second as i128;
        seconds -= (tz as i128) * 60;
        Some((seconds, self.nanos))
    }
}

impl Ord for XsdDateTime {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self.to_utc_seconds(), other.to_utc_seconds()) {
            (Some(a), Some(b)) => a.cmp(&b),
            _ => Ordering::Equal,
        }
    }
}
impl PartialOrd for XsdDateTime { fn partial_cmp(&self, o: &Self) -> Option<Ordering> { Some(self.cmp(o)) } }

/// Days from 0000-03-01 (proleptic Gregorian) to (y, m, d).  Standard
/// "civil from days" Hinnant algorithm.
fn days_from_civil(y: i32, m: u8, d: u8) -> i64 {
    let y = y as i64 - if m <= 2 { 1 } else { 0 };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as i64; // 0..399
    let m = m as i64;
    let d = d as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn ok<T>(r: Result<T, TypeError>) -> T {
        r.unwrap_or_else(|e| panic!("{e:?}"))
    }

    // ── dateTime ─────────────────────────────────────────────────────

    #[test]
    fn datetime_basic_no_tz() {
        let v = ok(parse_date_time("2024-03-15T10:30:00"));
        match v {
            Value::DateTime(d) => {
                assert_eq!((d.year, d.month, d.day), (2024, 3, 15));
                assert_eq!((d.hour, d.minute, d.second), (10, 30, 0));
                assert_eq!(d.tz_min, None);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn datetime_with_z() {
        let v = ok(parse_date_time("2024-03-15T10:30:00Z"));
        match v {
            Value::DateTime(d) => assert_eq!(d.tz_min, Some(0)),
            _ => panic!(),
        }
    }

    #[test]
    fn datetime_with_offset() {
        let v = ok(parse_date_time("2024-03-15T10:30:00-05:00"));
        match v {
            Value::DateTime(d) => assert_eq!(d.tz_min, Some(-300)),
            _ => panic!(),
        }
    }

    #[test]
    fn datetime_with_fractional_seconds() {
        let v = ok(parse_date_time("2024-03-15T10:30:45.123Z"));
        match v {
            Value::DateTime(d) => assert_eq!(d.nanos, 123_000_000),
            _ => panic!(),
        }
    }

    #[test]
    fn datetime_negative_year_allowed() {
        assert!(parse_date_time("-0044-03-15T00:00:00Z").is_ok());
    }

    #[test]
    fn datetime_year_zero_rejected() {
        assert!(parse_date_time("0000-01-01T00:00:00Z").is_err());
    }

    #[test]
    fn datetime_extended_year() {
        assert!(parse_date_time("12345-01-01T00:00:00Z").is_ok());
        // No leading zero on extended year.
        assert!(parse_date_time("01234-01-01T00:00:00Z").is_err());
    }

    #[test]
    fn datetime_24_00_00_allowed() {
        assert!(parse_date_time("2024-03-15T24:00:00Z").is_ok());
        // 24:30 is illegal.
        assert!(parse_date_time("2024-03-15T24:30:00Z").is_err());
    }

    #[test]
    fn datetime_invalid_day() {
        assert!(parse_date_time("2024-02-30T00:00:00Z").is_err());
        assert!(parse_date_time("2023-02-29T00:00:00Z").is_err()); // not leap
        assert!(parse_date_time("2024-02-29T00:00:00Z").is_ok());  // leap
    }

    #[test]
    fn datetime_tz_range() {
        assert!(parse_date_time("2024-01-01T00:00:00+14:00").is_ok());
        assert!(parse_date_time("2024-01-01T00:00:00-14:00").is_ok());
        assert!(parse_date_time("2024-01-01T00:00:00+15:00").is_err());
    }

    // ── date ─────────────────────────────────────────────────────────

    #[test]
    fn date_basic() {
        ok(parse_date("2024-03-15"));
        ok(parse_date("2024-03-15Z"));
        ok(parse_date("2024-03-15-08:00"));
    }

    #[test]
    fn date_rejects_time_part() {
        assert!(parse_date("2024-03-15T00:00:00").is_err());
    }

    // ── time ─────────────────────────────────────────────────────────

    #[test]
    fn time_basic() {
        ok(parse_time("10:30:00"));
        ok(parse_time("23:59:59.999Z"));
    }

    #[test]
    fn time_rejects_24_anything_but_zero() {
        assert!(parse_time("24:00:00").is_ok());
        assert!(parse_time("24:00:01").is_err());
        assert!(parse_time("25:00:00").is_err());
    }

    // ── gYearMonth / gYear / gMonthDay / gDay / gMonth ──────────────

    #[test]
    fn g_year_month() {
        ok(parse_g_year_month("2024-03"));
        ok(parse_g_year_month("2024-03Z"));
        assert!(parse_g_year_month("2024-13").is_err());
    }

    #[test]
    fn g_year() {
        ok(parse_g_year("2024"));
        ok(parse_g_year("-0044"));
        ok(parse_g_year("12345Z"));
    }

    #[test]
    fn g_month_day() {
        ok(parse_g_month_day("--03-15"));
        ok(parse_g_month_day("--02-29")); // permitted (leap year reference)
        assert!(parse_g_month_day("03-15").is_err());        // missing leading --
        assert!(parse_g_month_day("--13-01").is_err());      // bad month
    }

    #[test]
    fn g_day() {
        ok(parse_g_day("---15"));
        ok(parse_g_day("---01Z"));
        assert!(parse_g_day("--15").is_err());               // wrong number of dashes
    }

    #[test]
    fn g_month() {
        ok(parse_g_month("--12"));
        ok(parse_g_month("--01-08:00"));
        assert!(parse_g_month("12").is_err());
    }

    // ── duration ─────────────────────────────────────────────────────

    #[test]
    fn duration_basic() {
        let v = ok(parse_duration("P1Y2M3DT4H5M6S"));
        match v {
            Value::Duration(d) => {
                assert_eq!(d.months, 14);
                assert_eq!(d.seconds, 3*86400 + 4*3600 + 5*60 + 6);
                assert_eq!(d.nanos, 0);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn duration_negative() {
        let v = ok(parse_duration("-P1D"));
        match v {
            Value::Duration(d) => assert_eq!(d.seconds, -86400),
            _ => panic!(),
        }
    }

    #[test]
    fn duration_partial() {
        ok(parse_duration("PT1H"));
        ok(parse_duration("P1Y"));
        ok(parse_duration("PT0.5S"));
    }

    #[test]
    fn duration_rejects_empty() {
        assert!(parse_duration("P").is_err());
        assert!(parse_duration("PT").is_err());
        assert!(parse_duration("").is_err());
    }

    #[test]
    fn duration_fractional_seconds() {
        let v = ok(parse_duration("PT0.001S"));
        match v {
            Value::Duration(d) => {
                assert_eq!(d.seconds, 0);
                assert_eq!(d.nanos, 1_000_000);
            }
            _ => panic!(),
        }
    }

    // ── ordering ─────────────────────────────────────────────────────

    #[test]
    fn datetime_ordering_with_tz() {
        let earlier = match parse_date_time("2024-01-01T00:00:00Z").unwrap() {
            Value::DateTime(d) => d, _ => unreachable!()
        };
        let later = match parse_date_time("2024-01-02T00:00:00Z").unwrap() {
            Value::DateTime(d) => d, _ => unreachable!()
        };
        assert!(earlier < later);
    }

    #[test]
    fn datetime_ordering_normalizes_tz() {
        let utc_noon  = match parse_date_time("2024-01-01T12:00:00Z").unwrap() {
            Value::DateTime(d) => d, _ => unreachable!()
        };
        // 09:00 -03:00 = 12:00 UTC — same instant.
        let other  = match parse_date_time("2024-01-01T09:00:00-03:00").unwrap() {
            Value::DateTime(d) => d, _ => unreachable!()
        };
        assert_eq!(utc_noon.cmp(&other), Ordering::Equal);
    }
}
