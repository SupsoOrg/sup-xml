//! `format-number(number, picture, decimal-format?)` — XSLT 1.0 §12.3.
//!
//! Picture string grammar (XSLT 1.0 §12.3, §13.1.4):
//!
//! ```text
//! picture       := subpicture (';' subpicture)?
//! subpicture    := prefix? integer-part ('.' fraction-part)? suffix?
//! integer-part  := (digit-or-group)* (mandatory-digit)+
//! fraction-part := (mandatory-digit)* (optional-digit)*
//! ```
//!
//! Two subpictures: first is positive, second (after `;`) is
//! negative.  If no negative subpicture, the negative form prepends
//! the decimal-format's `minus-sign` to the positive form.
//!
//! The "characters" in a picture come from a [`DecimalFormat`]
//! (defaults documented in the struct).  Anything in the picture
//! that's NOT a special character is literal — copied to output as
//! a prefix or suffix.

/// Decimal-format settings, with `xsl:decimal-format` defaults.
/// Hand-wired here; named-`xsl:decimal-format` lookup from the
/// stylesheet isn't threaded through the function dispatcher yet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecimalFormat {
    pub decimal_separator: char,
    pub grouping_separator: char,
    pub infinity:          String,
    pub minus_sign:        char,
    pub nan:               String,
    pub percent:           char,
    pub per_mille:         char,
    pub zero_digit:        char,
    pub digit:             char,
    pub pattern_separator: char,
}

impl Default for DecimalFormat {
    fn default() -> Self {
        DecimalFormat {
            decimal_separator: '.',
            grouping_separator: ',',
            infinity:           "Infinity".to_string(),
            minus_sign:         '-',
            nan:                "NaN".to_string(),
            percent:            '%',
            per_mille:          '\u{2030}',
            zero_digit:         '0',
            digit:              '#',
            pattern_separator:  ';',
        }
    }
}

/// Format `value` per `picture` using `df`.  Returns the formatted
/// string, or an XTDE1310 message when the picture violates the
/// rules of XSLT 2.0 §16.4.3.
pub fn format_number(value: f64, picture: &str, df: &DecimalFormat)
    -> Result<String, String>
{
    if value.is_nan() { return Ok(df.nan.clone()); }
    validate_picture(picture, df)?;

    // Split into positive / negative subpictures at the
    // pattern-separator char (default `;`).  Pattern-separator
    // chars inside a quoted literal in the picture are NOT split
    // points; XSLT picture strings don't have quoting in 1.0, so
    // we split unconditionally.
    let mut parts = picture.splitn(2, df.pattern_separator);
    let pos_pic = parts.next().unwrap_or("");
    let neg_pic = parts.next();

    let pic = if value < 0.0 && neg_pic.is_some() {
        neg_pic.unwrap()
    } else {
        pos_pic
    };
    let positive_pic = parse_subpicture(pic, df);
    let abs = if value < 0.0 { -value } else { value };

    let formatted = format_subpicture(abs, &positive_pic, df);

    // If no explicit negative subpicture, prepend the minus sign
    // for negative inputs.
    if value < 0.0 && neg_pic.is_none() {
        return Ok(format!("{}{}", df.minus_sign, formatted));
    }
    Ok(formatted)
}

/// XSLT 2.0 §16.4.3 picture-string validation.  Each subpicture
/// must contain at least one digit-character (zero-digit or
/// digit-`#`), `#` digits in the integer part may not appear after
/// a zero-digit, and `0` digits in the fraction part may not
/// appear after a `#`.  Any violation is XTDE1310.
fn validate_picture(picture: &str, df: &DecimalFormat) -> Result<(), String> {
    // XSLT 2.0 §16.4.3 — at most two subpictures separated by the
    // pattern-separator.  Three or more is XTDE1310.
    let parts: Vec<&str> = picture.split(df.pattern_separator).collect();
    if parts.len() > 2 {
        return Err(format!(
            "format-number picture '{picture}' has more than two subpictures \
             (XTDE1310)"
        ));
    }
    for p in &parts { validate_subpicture(p, df)?; }
    Ok(())
}

fn validate_subpicture(s: &str, df: &DecimalFormat) -> Result<(), String> {
    if s.is_empty() {
        return Err(format!(
            "format-number picture '{s}' has an empty subpicture (XTDE1310)"
        ));
    }
    // XSLT 2.0 §16.4.3 — a subpicture may contain at most one
    // decimal-separator and at most one percent / per-mille
    // (and not both).  Multiple percent OR multiple per-mille OR
    // a mix of the two is XTDE1310.
    let dec_count    = s.matches(df.decimal_separator).count();
    let percent_n    = s.matches(df.percent).count();
    let per_mille_n  = s.matches(df.per_mille).count();
    if dec_count > 1 {
        return Err(format!(
            "format-number picture '{s}' contains more than one \
             decimal-separator (XTDE1310)"
        ));
    }
    if percent_n + per_mille_n > 1 {
        return Err(format!(
            "format-number picture '{s}' contains more than one \
             percent / per-mille marker (XTDE1310)"
        ));
    }
    // Walk the subpicture in two halves split by the decimal-
    // separator (if any), enforcing the per-side ordering rules.
    let halves: Vec<&str> = s.splitn(2, df.decimal_separator).collect();
    let int_part  = halves[0];
    let frac_part = halves.get(1).copied().unwrap_or("");
    // §16.4.3 — a grouping-separator may not appear adjacent to the
    // decimal-separator (`0,.0` is invalid even though both halves
    // contain digits).
    if int_part.ends_with(df.grouping_separator) && halves.len() > 1 {
        return Err(format!(
            "format-number picture '{s}' has a grouping-separator \
             adjacent to the decimal-separator (XTDE1310)"
        ));
    }
    if frac_part.starts_with(df.grouping_separator) {
        return Err(format!(
            "format-number picture '{s}' has a grouping-separator \
             adjacent to the decimal-separator (XTDE1310)"
        ));
    }

    // Integer side: zero-digit may not precede the optional-digit
    // sign.  Once we've seen a `0`, every subsequent digit char
    // must also be `0`.
    let mut saw_zero = false;
    let mut int_digits = 0usize;
    for c in int_part.chars() {
        if c == df.zero_digit {
            saw_zero = true;
            int_digits += 1;
        } else if c == df.digit {
            if saw_zero {
                return Err(format!(
                    "format-number picture '{s}' has '#' after '0' in the \
                     integer part (XTDE1310)"
                ));
            }
            int_digits += 1;
        }
        // Other chars (grouping-separator / prefix-suffix) are
        // legal placeholders here.
    }

    // Fraction side: `0` may not appear AFTER `#`.  Walk left-to-
    // right; once we've seen a `#`, every subsequent digit char
    // must also be `#`.
    let mut saw_optional = false;
    let mut frac_digits = 0usize;
    for c in frac_part.chars() {
        if c == df.digit {
            saw_optional = true;
            frac_digits += 1;
        } else if c == df.zero_digit {
            if saw_optional {
                return Err(format!(
                    "format-number picture '{s}' has '0' after '#' in the \
                     fraction part (XTDE1310)"
                ));
            }
            frac_digits += 1;
        }
    }

    if int_digits + frac_digits == 0 {
        return Err(format!(
            "format-number picture '{s}' contains no digit characters \
             (XTDE1310)"
        ));
    }
    // §16.4.3 — once the integer "active part" starts (first
    // digit-character), only digit-characters, the grouping-separator,
    // and the decimal-separator may appear up to the active part's
    // end (i.e. until a non-digit, non-grouping suffix character).
    // The same applies to the fraction part.  Detect a stray
    // non-placeholder mid-stream — `0$0` is the canonical failure.
    fn check_active(part: &str, df: &DecimalFormat, who: &str)
        -> Result<(), String>
    {
        let mut in_active = false;
        let mut left_active = false;
        for c in part.chars() {
            let is_digit_char = c == df.zero_digit || c == df.digit;
            let is_sep = c == df.grouping_separator;
            if !in_active && is_digit_char {
                in_active = true;
                continue;
            }
            if in_active && !left_active {
                if is_digit_char || is_sep { continue; }
                // Anything else closes the active part — but a
                // non-passive character (digit or grouping-separator)
                // after it would mean the picture splits the active
                // run with a foreign character, which §16.4.3
                // rejects as XTDE1310.
                left_active = true;
                continue;
            }
            if left_active && (is_digit_char || is_sep) {
                return Err(format!(
                    "format-number picture '{}' has a non-placeholder \
                     character splitting the {who} active part (XTDE1310)",
                    part
                ));
            }
        }
        Ok(())
    }
    check_active(int_part,  df, "integer")?;
    check_active(frac_part, df, "fraction")?;
    Ok(())
}

#[derive(Debug, Clone)]
struct SubPicture {
    prefix:           String,
    suffix:           String,
    /// Minimum digits before the decimal (count of `zero_digit` in
    /// the integer part).
    min_integer:      usize,
    /// Regular grouping interval — set when the picture's grouping
    /// separators are evenly spaced (the common `#,##0` case), so a
    /// separator repeats every `group_size` digits including for numbers
    /// wider than the picture.
    group_size:       Option<usize>,
    /// Irregular grouping positions (distance from the right edge of the
    /// integer part) for pictures like `###,##0,00` whose separators are
    /// NOT evenly spaced.  XSLT 3.0 §4.7.4: when grouping is irregular the
    /// separators appear ONLY at these positions, never repeated beyond
    /// the leftmost.  Empty when `group_size` carries a regular interval.
    group_positions:  Vec<usize>,
    /// Minimum digits after the decimal (count of `zero_digit` in
    /// the fraction part).
    min_fraction:     usize,
    /// Maximum digits after the decimal (count of `zero_digit` +
    /// `digit` in the fraction part).
    max_fraction:     usize,
    /// `*100` (percent) or `*1000` (per-mille) scale applied
    /// before formatting.  XSLT 1.0 says the suffix's % or ‰
    /// chars trigger this.
    scale:            f64,
}

fn parse_subpicture(s: &str, df: &DecimalFormat) -> SubPicture {
    let mut prefix = String::new();
    let mut suffix = String::new();
    let mut min_integer = 0;
    let mut group_size = None;
    // Left-edge positions (count of integer digits seen so far) at which
    // a grouping separator appeared, in picture order.
    let mut group_marks: Vec<usize> = Vec::new();
    let mut min_fraction = 0;
    let mut max_fraction = 0;
    let mut scale = 1.0;

    enum Phase { Prefix, Integer, Fraction, Suffix }
    let mut phase = Phase::Prefix;
    let mut integer_digits = 0usize;
    let mut fraction_digits = 0usize;
    for c in s.chars() {
        match phase {
            Phase::Prefix => {
                if c == df.zero_digit || c == df.digit {
                    phase = Phase::Integer;
                    integer_digits += 1;
                    if c == df.zero_digit { min_integer += 1; }
                } else if c == df.grouping_separator {
                    // A grouping separator opens the active integer part;
                    // its left position is 0 (no digits seen yet).
                    phase = Phase::Integer;
                    group_marks.push(integer_digits);
                } else {
                    prefix.push(c);
                }
            }
            Phase::Integer => {
                if c == df.zero_digit || c == df.digit {
                    integer_digits += 1;
                    if c == df.zero_digit { min_integer += 1; }
                } else if c == df.grouping_separator {
                    group_marks.push(integer_digits);
                } else if c == df.decimal_separator {
                    phase = Phase::Fraction;
                } else {
                    phase = Phase::Suffix;
                    suffix.push(c);
                    classify_suffix_char(c, df, &mut scale);
                }
            }
            Phase::Fraction => {
                if c == df.zero_digit {
                    fraction_digits += 1; min_fraction += 1; max_fraction += 1;
                } else if c == df.digit {
                    fraction_digits += 1; max_fraction += 1;
                } else {
                    phase = Phase::Suffix;
                    suffix.push(c);
                    classify_suffix_char(c, df, &mut scale);
                }
            }
            Phase::Suffix => {
                suffix.push(c);
                classify_suffix_char(c, df, &mut scale);
            }
        }
    }
    let _ = fraction_digits;
    // Convert each separator's left position to a distance from the right
    // edge of the integer part.  A trailing separator (e.g. picture `#,`)
    // sits at distance 0 and is dropped — it would otherwise mean a zero
    // group size (divide-by-zero) on malformed input.
    let mut positions: Vec<usize> = group_marks.iter()
        .map(|&m| integer_digits.saturating_sub(m))
        .filter(|&d| d > 0)
        .collect();
    positions.sort_unstable();
    positions.dedup();
    let mut group_positions = Vec::new();
    // Grouping is REGULAR when the separator positions are exactly the
    // multiples d, 2d, 3d, … of the smallest position; then a single
    // interval applies (and repeats for wider numbers).  Otherwise the
    // separators are honoured only at the positions given (XSLT 3.0
    // §4.7.4).
    if let Some(&d) = positions.first() {
        let regular = positions.iter().enumerate()
            .all(|(i, &p)| p == (i + 1) * d);
        if regular {
            group_size = Some(d);
        } else {
            group_positions = positions;
        }
    }
    SubPicture {
        prefix, suffix, min_integer, group_size, group_positions,
        min_fraction, max_fraction, scale,
    }
}

fn classify_suffix_char(c: char, df: &DecimalFormat, scale: &mut f64) {
    if c == df.percent     { *scale *= 100.0; }
    else if c == df.per_mille { *scale *= 1000.0; }
}

fn format_subpicture(abs: f64, p: &SubPicture, df: &DecimalFormat) -> String {
    if abs.is_infinite() {
        return format!("{}{}{}", p.prefix, df.infinity, p.suffix);
    }
    let scaled = abs * p.scale;
    // Round to `max_fraction` digits using XPath's half-to-even
    // behaviour — XSLT spec is silent so we match libxslt's
    // half-up.
    let factor = 10f64.powi(p.max_fraction as i32);
    let rounded = (scaled * factor).round() / factor;

    // Split int / frac as strings to preserve trailing zeros.
    let mut int_part: u64 = rounded.trunc() as u64;
    let frac_value = rounded - int_part as f64;
    let frac_str = if p.max_fraction == 0 {
        String::new()
    } else {
        let f_int = (frac_value * factor).round() as u64;
        let mut s = format!("{:0width$}", f_int, width = p.max_fraction);
        // Trim trailing zeros down to min_fraction.
        while s.len() > p.min_fraction && s.ends_with('0') {
            s.pop();
        }
        // Remap ASCII fraction digits onto the decimal-format's zero
        // digit base (matches what we do for integer digits) so locales
        // with non-ASCII digits get consistent fraction output too.
        if df.zero_digit != '0' {
            let zero_code = df.zero_digit as u32;
            s = s.chars().map(|c| match c.to_digit(10) {
                Some(d) => char::from_u32(zero_code + d).unwrap_or(c),
                None    => c,
            }).collect();
        }
        s
    };

    // Build integer string with grouping.
    let mut int_digits: Vec<char> = if int_part == 0 {
        vec![df.zero_digit]
    } else {
        let mut v = Vec::new();
        while int_part > 0 {
            v.push(char::from_digit((int_part % 10) as u32, 10).unwrap());
            int_part /= 10;
        }
        v.reverse();
        // Map ASCII digits to df.zero_digit-based digits if non-ASCII.
        if df.zero_digit != '0' {
            let zero_code = df.zero_digit as u32;
            for c in &mut v {
                let d = c.to_digit(10).unwrap();
                *c = char::from_u32(zero_code + d).unwrap_or(*c);
            }
        }
        v
    };
    // Pad to min_integer.
    while int_digits.len() < p.min_integer {
        int_digits.insert(0, df.zero_digit);
    }
    // Group.
    let mut int_str = String::new();
    let n = int_digits.len();
    if let Some(g) = p.group_size {
        for (i, c) in int_digits.iter().enumerate() {
            if i > 0 && (n - i) % g == 0 {
                int_str.push(df.grouping_separator);
            }
            int_str.push(*c);
        }
    } else if !p.group_positions.is_empty() {
        for (i, c) in int_digits.iter().enumerate() {
            if i > 0 && p.group_positions.contains(&(n - i)) {
                int_str.push(df.grouping_separator);
            }
            int_str.push(*c);
        }
    } else {
        int_str = int_digits.iter().collect();
    }

    let mut out = String::new();
    out.push_str(&p.prefix);
    out.push_str(&int_str);
    if !frac_str.is_empty() {
        out.push(df.decimal_separator);
        out.push_str(&frac_str);
    }
    out.push_str(&p.suffix);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn df() -> DecimalFormat { DecimalFormat::default() }

    #[test]
    fn integer_with_zero_min() {
        assert_eq!(format_number(1234.0, "0", &df()).unwrap(),     "1234");
        assert_eq!(format_number(1.0,    "0000", &df()).unwrap(),  "0001");
    }

    #[test]
    fn integer_with_grouping() {
        assert_eq!(format_number(1234567.0, "#,##0", &df()).unwrap(), "1,234,567");
    }

    #[test]
    fn leading_grouping_separator_sets_interval() {
        // A grouping separator before the first digit opens the active
        // part and fixes a regular interval (group size 3 here).
        assert_eq!(format_number(1234567890.0, ",000", &df()).unwrap(),
                   "1,234,567,890");
    }

    #[test]
    fn unequal_grouping_uses_exact_positions() {
        // XSLT 3.0 §4.7.4 — irregular separators (here at 2 and 5 digits
        // from the right) appear only where the picture places them and
        // are NOT repeated beyond the leftmost.
        assert_eq!(format_number(987654321.0, "###,##0,00.00", &df()).unwrap(),
                   "9876,543,21.00");
    }

    #[test]
    fn fraction_padded_to_min() {
        assert_eq!(format_number(1.5, "0.00", &df()).unwrap(), "1.50");
    }

    #[test]
    fn fraction_trimmed_to_min() {
        assert_eq!(format_number(1.5, "0.0##", &df()).unwrap(), "1.5");
    }

    #[test]
    fn fraction_rounds_at_max() {
        assert_eq!(format_number(1.2345, "0.00", &df()).unwrap(), "1.23");
        assert_eq!(format_number(1.235,  "0.00", &df()).unwrap(), "1.24"); // half-up
    }

    #[test]
    fn negative_uses_implicit_minus_prefix() {
        assert_eq!(format_number(-42.0, "0", &df()).unwrap(), "-42");
    }

    #[test]
    fn negative_uses_explicit_subpicture() {
        assert_eq!(format_number(-42.0, "0;(0)", &df()).unwrap(), "(42)");
    }

    #[test]
    fn nan_returns_nan_literal() {
        assert_eq!(format_number(f64::NAN, "0", &df()).unwrap(), "NaN");
    }

    #[test]
    fn percent_scales_by_100() {
        assert_eq!(format_number(0.25, "0%", &df()).unwrap(), "25%");
    }
}
