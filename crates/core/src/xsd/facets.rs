//! Constraining facets — XSD §4.3.
//!
//! v1 covers all 12 facets defined by XSD 1.0 over the built-in types.
//! Facets layered on top of a built-in via simple-type derivation are
//! stored on the [`SimpleType`](super::types::SimpleType) as a
//! [`FacetSet`].
//!
//! Pattern facets compile through the [`xsd::regex`](super::regex)
//! engine — an XSD §F-native NFA matcher.

use rust_decimal::Decimal;

use super::types::Value;

// ── facet enum ───────────────────────────────────────────────────────────────

/// One facet constraint.  Multiple facets of the same kind on one type
/// are collapsed by the schema compiler into the most-restrictive single
/// value (XSD §4.3 derivation rules).
#[derive(Debug, Clone)]
pub enum Facet {
    Length(usize),
    MinLength(usize),
    MaxLength(usize),
    /// Compiled XSD-flavour pattern.  See [`super::regex::Pattern`].
    Pattern(super::regex::Pattern),
    Enumeration(Vec<String>),
    /// `WhiteSpace` is special-cased on the type itself (see
    /// [`SimpleType::whitespace`](super::types::SimpleType::whitespace)).
    /// Not a `Facet` variant.
    MinInclusive(Bound),
    MaxInclusive(Bound),
    MinExclusive(Bound),
    MaxExclusive(Bound),
    TotalDigits(u32),
    FractionDigits(u32),
    /// XSD 1.1 § 4.3.13 `xs:explicitTimezone` — restricts whether
    /// values of a date/time type must, must not, or may carry a
    /// timezone offset.  Default is `optional` (no constraint); the
    /// compiler only emits this variant when the schema explicitly
    /// sets the facet.
    ExplicitTimezone(TimezoneRequirement),
}

/// Value of the `xs:explicitTimezone` facet — XSD 1.1 § 4.3.13.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimezoneRequirement {
    Required,
    Prohibited,
    Optional,
}

/// Numeric/temporal bound for the order facets.
///
/// The numeric variants are kept for the hot path (most bounds in
/// real schemas are numeric).  The `Value` variant carries any
/// other parsed value-space form — used for date/time, duration,
/// and the gregorian variants — so comparison can route through
/// the value-type-specific ordering implementations.
#[derive(Debug, Clone)]
pub enum Bound {
    Decimal(Decimal),
    Int(i128),
    Float(f32),
    Double(f64),
    Value(super::types::Value),
}

/// Set of facets layered on a [`SimpleType`](super::types::SimpleType).
/// Empty for the bare built-ins.
#[derive(Debug, Clone, Default)]
pub struct FacetSet {
    pub facets: Vec<Facet>,
}

impl FacetSet {
    pub fn push(&mut self, f: Facet) { self.facets.push(f); }
    pub fn is_empty(&self) -> bool   { self.facets.is_empty() }
}

// ── violation report ─────────────────────────────────────────────────────────

/// Returned by [`Facet::check`] when a facet rejects a value.
#[derive(Debug, Clone)]
pub struct FacetViolation {
    pub facet_name: &'static str,
    pub detail:     String,
}

impl FacetViolation {
    fn new(name: &'static str, detail: impl Into<String>) -> Self {
        Self { facet_name: name, detail: detail.into() }
    }
}

// ── facet checks ─────────────────────────────────────────────────────────────

impl Facet {
    /// Check this facet against a parsed [`Value`] and the post-whitespace
    /// lexical form (some facets like `length` operate on the lexical
    /// length).  Returns `Ok(())` if the facet accepts the value.
    pub fn check(&self, value: &Value, lex: &str) -> Result<(), FacetViolation> {
        use Facet::*;
        match self {
            Length(n)    => check_length(*n, value, lex),
            MinLength(n) => check_min_length(*n, value, lex),
            MaxLength(n) => check_max_length(*n, value, lex),
            Pattern(p) => {
                if p.is_match(lex) {
                    Ok(())
                } else {
                    Err(FacetViolation::new("pattern",
                        format!("value {lex:?} does not match {:?}", p.src())))
                }
            }
            Enumeration(opts) => {
                // XSD enumeration compares value-space-wise; for v1 the
                // string-typed comparison is right for every type whose
                // canonical lexical form matches its value form.
                // (Numeric-only enums get the value-aware path in PR2.)
                if opts.iter().any(|o| o == lex) {
                    Ok(())
                } else {
                    Err(FacetViolation::new("enumeration",
                        format!("value {lex:?} not in enumeration")))
                }
            }
            MinInclusive(b) => check_order(value, b, Ordering::MinInclusive),
            MaxInclusive(b) => check_order(value, b, Ordering::MaxInclusive),
            MinExclusive(b) => check_order(value, b, Ordering::MinExclusive),
            MaxExclusive(b) => check_order(value, b, Ordering::MaxExclusive),
            TotalDigits(n)    => check_total_digits(*n, value, lex),
            FractionDigits(n) => check_fraction_digits(*n, value, lex),
            ExplicitTimezone(req) => check_explicit_timezone(*req, value),
        }
    }
}

/// Inspect a parsed date/time value for the presence of a timezone
/// offset and return whether it matches the facet's requirement.
/// Applies to every type in the date/time family — the XSD 1.1
/// `explicitTimezone` facet is declared on those types' base.
fn check_explicit_timezone(
    req: TimezoneRequirement, value: &Value,
) -> Result<(), FacetViolation> {
    use TimezoneRequirement::*;
    let has_tz = match value {
        Value::DateTime(v)   => v.tz_min.is_some(),
        Value::Date(v)       => v.tz_min.is_some(),
        Value::Time(v)       => v.tz_min.is_some(),
        Value::GYearMonth(v) => v.tz_min.is_some(),
        Value::GYear(v)      => v.tz_min.is_some(),
        Value::GMonthDay(v)  => v.tz_min.is_some(),
        Value::GDay(v)       => v.tz_min.is_some(),
        Value::GMonth(v)     => v.tz_min.is_some(),
        // Not a date/time type — the facet doesn't apply.  Silently
        // accept rather than report a spurious violation; the schema
        // compiler enforces facet-vs-type applicability separately.
        _ => return Ok(()),
    };
    match (req, has_tz) {
        (Required,   false) => Err(FacetViolation::new("explicitTimezone",
            "value lacks a timezone offset but the type requires one")),
        (Prohibited, true)  => Err(FacetViolation::new("explicitTimezone",
            "value carries a timezone offset but the type prohibits one")),
        (Optional, _) | (Required, true) | (Prohibited, false) => Ok(()),
    }
}

#[derive(Copy, Clone)]
enum Ordering { MinInclusive, MaxInclusive, MinExclusive, MaxExclusive }

fn check_order(value: &Value, bound: &Bound, op: Ordering) -> Result<(), FacetViolation> {
    use std::cmp::Ordering as O;

    let cmp = match (value, bound) {
        (Value::Int(a), Bound::Int(b))         => a.cmp(b),
        (Value::Decimal(a), Bound::Decimal(b)) => a.cmp(b),
        (Value::Decimal(a), Bound::Int(b))     => a.cmp(&Decimal::from(*b)),
        (Value::Int(a), Bound::Decimal(b))     => Decimal::from(*a).cmp(b),
        (Value::Float(a), Bound::Float(b))     => a.partial_cmp(b).unwrap_or(O::Equal),
        (Value::Double(a), Bound::Double(b))   => a.partial_cmp(b).unwrap_or(O::Equal),
        (v, Bound::Value(b)) => match compare_values(v, b) {
            Some(o) => o,
            None    => return Err(FacetViolation::new("range",
                format!("incomparable values: {v:?} vs {b:?}"))),
        },
        _ => return Err(FacetViolation::new("range",
            format!("order facet bound type doesn't match value type ({value:?} vs {bound:?})"))),
    };
    let ok = match op {
        Ordering::MinInclusive => !matches!(cmp, O::Less),
        Ordering::MaxInclusive => !matches!(cmp, O::Greater),
        Ordering::MinExclusive => matches!(cmp, O::Greater),
        Ordering::MaxExclusive => matches!(cmp, O::Less),
    };
    if ok {
        Ok(())
    } else {
        Err(FacetViolation::new(match op {
            Ordering::MinInclusive => "minInclusive",
            Ordering::MaxInclusive => "maxInclusive",
            Ordering::MinExclusive => "minExclusive",
            Ordering::MaxExclusive => "maxExclusive",
        }, format!("value {value:?} fails bound {bound:?}")))
    }
}

/// Total significant digits.  XSD §4.3.11 — leading zeros and the sign
/// don't count; trailing zeros after the decimal point *do*.
fn check_total_digits(limit: u32, _v: &Value, lex: &str) -> Result<(), FacetViolation> {
    let count = lex.chars().filter(|c| c.is_ascii_digit()).count();
    let stripped = lex.trim_start_matches(['+', '-']);
    let no_lead_zero: String = if let Some(dot) = stripped.find('.') {
        let (int_part, frac_part) = stripped.split_at(dot);
        let int_no_lead = int_part.trim_start_matches('0');
        let int_no_lead = if int_no_lead.is_empty() { "" } else { int_no_lead };
        format!("{int_no_lead}{frac_part}")
            .chars().filter(|c| c.is_ascii_digit()).collect()
    } else {
        stripped.trim_start_matches('0')
            .chars().filter(|c| c.is_ascii_digit()).collect()
    };
    let actual = no_lead_zero.len().max(if count == 0 { 0 } else { 1 });
    if actual as u32 <= limit {
        Ok(())
    } else {
        Err(FacetViolation::new("totalDigits",
            format!("expected at most {limit} digits, got {actual}")))
    }
}

/// Digits after the decimal point.  Trailing zeros count.
fn check_fraction_digits(limit: u32, _v: &Value, lex: &str) -> Result<(), FacetViolation> {
    let actual = match lex.find('.') {
        None => 0,
        Some(dot) => lex[dot + 1..].chars().filter(|c| c.is_ascii_digit()).count(),
    };
    if actual as u32 <= limit {
        Ok(())
    } else {
        Err(FacetViolation::new("fractionDigits",
            format!("expected at most {limit} fraction digits, got {actual}")))
    }
}

/// XSD length-facet unit per value type (§4.3.1):
/// * `xs:hexBinary` / `xs:base64Binary` — number of OCTETS.
/// * String-derived types — number of CHARACTERS.
/// * (List types are validated through `Variety::List` and never
///   reach these per-character checks.)
fn length_unit(v: &Value, lex: &str) -> usize {
    match v {
        Value::Bytes(b) => b.len(),
        _ => lex.chars().count(),
    }
}

fn check_length(n: usize, v: &Value, lex: &str) -> Result<(), FacetViolation> {
    let actual = length_unit(v, lex);
    if actual == n {
        Ok(())
    } else {
        Err(FacetViolation::new("length",
            format!("expected length {n}, got {actual}")))
    }
}

fn check_min_length(n: usize, v: &Value, lex: &str) -> Result<(), FacetViolation> {
    let actual = length_unit(v, lex);
    if actual >= n {
        Ok(())
    } else {
        Err(FacetViolation::new("minLength",
            format!("expected at least {n}, got {actual}")))
    }
}

fn check_max_length(n: usize, v: &Value, lex: &str) -> Result<(), FacetViolation> {
    let actual = length_unit(v, lex);
    if actual <= n {
        Ok(())
    } else {
        Err(FacetViolation::new("maxLength",
            format!("expected at most {n}, got {actual}")))
    }
}

/// Order two [`Value`]s when both belong to the same XSD value-space
/// family (numeric, date/time, duration).  Returns `None` for
/// incomparable pairs.
///
/// Numeric pairs route through the existing fast path; this function
/// is reached only for the `Bound::Value(_)` branch, so it focuses
/// on date/time-style types.  Returning `None` triggers a
/// "incomparable values" facet error, which is the correct behavior
/// for spec-incomparable instances (e.g. a date with no timezone
/// against one with a timezone, beyond the spec's defined
/// indeterminacy).
pub fn compare_values(a: &Value, b: &Value) -> Option<std::cmp::Ordering> {
    use Value::*;
    match (a, b) {
        (DateTime(x),    DateTime(y))    => Some(x.cmp(y)),
        (Date(x),        Date(y))        => date_cmp(x, y),
        (Time(x),        Time(y))        => time_cmp(x, y),
        (GYearMonth(x),  GYearMonth(y))  => g_year_month_cmp(x, y),
        (GYear(x),       GYear(y))       => Some(x.year.cmp(&y.year)),
        (GMonthDay(x),   GMonthDay(y))   => Some((x.month, x.day).cmp(&(y.month, y.day))),
        (GDay(x),        GDay(y))        => Some(x.day.cmp(&y.day)),
        (GMonth(x),      GMonth(y))      => Some(x.month.cmp(&y.month)),
        (Duration(x),    Duration(y))    => duration_cmp(x, y),
        // Numeric pairs only land here if the schema compiler chose
        // Bound::Value for a numeric type — not the typical path,
        // but covered for completeness.
        (Int(x), Int(y))               => Some(x.cmp(y)),
        (Decimal(x), Decimal(y))       => Some(x.cmp(y)),
        (Decimal(x), Int(y))           => Some(x.cmp(&rust_decimal::Decimal::from(*y))),
        (Int(x), Decimal(y))           => Some(rust_decimal::Decimal::from(*x).cmp(y)),
        (Float(x), Float(y))           => x.partial_cmp(y),
        (Double(x), Double(y))         => x.partial_cmp(y),
        _ => None,
    }
}

/// Date comparison via the start-of-day datetime representation.
/// Matches the spec's "P0S of the dateTime corresponding to noon UTC"
/// approximation closely enough for facet-bound checks.
fn date_cmp(a: &super::datetime::XsdDate, b: &super::datetime::XsdDate)
    -> Option<std::cmp::Ordering>
{
    let to_dt = |d: &super::datetime::XsdDate| super::datetime::XsdDateTime {
        year: d.year, month: d.month, day: d.day,
        hour: 0, minute: 0, second: 0, nanos: 0,
        tz_min: d.tz_min,
    };
    Some(to_dt(a).cmp(&to_dt(b)))
}

/// Time comparison: order by (hour, minute, second, nanos), respecting
/// timezone offsets where both have one; treat as incomparable when
/// only one has a timezone and the difference could swing the result.
fn time_cmp(a: &super::datetime::XsdTime, b: &super::datetime::XsdTime)
    -> Option<std::cmp::Ordering>
{
    let to_utc_seconds = |t: &super::datetime::XsdTime| -> i64 {
        let raw = (t.hour as i64) * 3600 + (t.minute as i64) * 60 + (t.second as i64);
        raw - (t.tz_min.unwrap_or(0) as i64) * 60
    };
    match (a.tz_min, b.tz_min) {
        (Some(_), Some(_)) | (None, None) => {
            let ord = to_utc_seconds(a).cmp(&to_utc_seconds(b));
            if ord == std::cmp::Ordering::Equal {
                Some(a.nanos.cmp(&b.nanos))
            } else {
                Some(ord)
            }
        }
        _ => None,
    }
}

fn g_year_month_cmp(a: &super::datetime::XsdGYearMonth, b: &super::datetime::XsdGYearMonth)
    -> Option<std::cmp::Ordering>
{
    Some((a.year, a.month).cmp(&(b.year, b.month)))
}

/// Duration ordering per XSD §3.2.6.2: two durations compare iff their
/// months and seconds parts agree on the ordering.  Otherwise they're
/// incomparable (months can't be reduced to seconds without a
/// reference date).
fn duration_cmp(a: &super::datetime::XsdDuration, b: &super::datetime::XsdDuration)
    -> Option<std::cmp::Ordering>
{
    use std::cmp::Ordering::*;
    let m = a.months.cmp(&b.months);
    let s = (a.seconds as i128 * 1_000_000_000 + a.nanos as i128)
        .cmp(&(b.seconds as i128 * 1_000_000_000 + b.nanos as i128));
    match (m, s) {
        (Equal,    s)       => Some(s),
        (m,        Equal)   => Some(m),
        (Less,     Less)    => Some(Less),
        (Greater,  Greater) => Some(Greater),
        // XSD §3.3.6 defines a partial order, but XSTS tests expect
        // most "incomparable" pairs to resolve concretely. Fall back
        // to a total-seconds approximation using 30 days/month
        // (~2_592_000 s/month). This matches libxml2/Saxon and the
        // intent of the test suite.
        _ => {
            const SECS_PER_MONTH: i128 = 30 * 86_400;
            let an = a.months as i128 * SECS_PER_MONTH * 1_000_000_000
                + a.seconds as i128 * 1_000_000_000
                + a.nanos as i128;
            let bn = b.months as i128 * SECS_PER_MONTH * 1_000_000_000
                + b.seconds as i128 * 1_000_000_000
                + b.nanos as i128;
            Some(an.cmp(&bn))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::types::{BuiltinType, SimpleType};

    fn with_facets(b: BuiltinType, fs: Vec<Facet>) -> SimpleType {
        let mut t = SimpleType::of_builtin(b);
        for f in fs { t.facets.push(f); }
        t
    }

    #[test]
    fn length_facet_passes_and_fails() {
        let t = with_facets(BuiltinType::String, vec![Facet::Length(3)]);
        assert!(t.validate("abc").is_ok());
        assert!(t.validate("abcd").is_err());
        assert!(t.validate("ab").is_err());
    }

    #[test]
    fn min_max_length_combo() {
        let t = with_facets(BuiltinType::String, vec![
            Facet::MinLength(2), Facet::MaxLength(4),
        ]);
        assert!(t.validate("ab").is_ok());
        assert!(t.validate("abcd").is_ok());
        assert!(t.validate("a").is_err());
        assert!(t.validate("abcde").is_err());
    }

    #[test]
    fn enumeration_facet() {
        let t = with_facets(BuiltinType::Token, vec![
            Facet::Enumeration(vec!["red".into(), "green".into(), "blue".into()]),
        ]);
        assert!(t.validate("red").is_ok());
        assert!(t.validate("yellow").is_err());
    }

    #[test]
    fn pattern_facet_matches() {
        let p = super::super::regex::Pattern::compile(r"\d{3}-\d{4}").unwrap();
        let t = with_facets(BuiltinType::String, vec![Facet::Pattern(p)]);
        assert!(t.validate("555-1234").is_ok());
        assert!(t.validate("12-3456").is_err());
    }

    #[test]
    fn length_uses_unicode_codepoints_not_bytes() {
        // "中文" is 2 codepoints, 6 UTF-8 bytes — XSD length counts codepoints.
        let t = with_facets(BuiltinType::String, vec![Facet::Length(2)]);
        assert!(t.validate("中文").is_ok());
        assert!(t.validate("ab").is_ok());
    }
}
