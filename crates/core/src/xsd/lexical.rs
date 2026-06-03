//! Lexical-space parsers for the built-in datatypes — numerics first,
//! then date/time, then binary/URI/QName.
//!
//! Each parser takes the post-whitespace lexical value and returns a
//! [`Value`] (value-space representation).  Per-type range checks live
//! here too — `xs:byte` validates as a number first, then constrains
//! to `i8::MIN..=i8::MAX`.

use std::str::FromStr;

use rust_decimal::Decimal;

use super::types::{TypeError, Value, num_overflow::BigInt};

// ── decimal ──────────────────────────────────────────────────────────────────

/// Parse `xs:decimal`.  Accepts an optional sign, an integer part, and an
/// optional fractional part.  Rejects scientific notation (that's `float`).
pub fn parse_decimal(s: &str) -> Result<Value, TypeError> {
    if s.is_empty() {
        return Err(TypeError::type_mismatch("decimal cannot be empty"));
    }
    // Validate the lexical form ourselves — `Decimal::from_str` accepts
    // some forms XSD doesn't (like leading `+` it accepts, `1e2` it
    // rejects → fine, but we need to match XSD exactly).
    if !is_decimal_lexical(s) {
        return Err(TypeError::type_mismatch(format!("invalid decimal: {s:?}")));
    }
    Decimal::from_str(s)
        .map(Value::Decimal)
        .map_err(|e| TypeError::type_mismatch(format!("invalid decimal {s:?}: {e}")))
}

fn is_decimal_lexical(s: &str) -> bool {
    let bytes = s.as_bytes();
    let mut i = 0;
    if matches!(bytes.first(), Some(b'+' | b'-')) { i += 1; }
    let mut saw_digit = false;
    let mut saw_dot   = false;
    while i < bytes.len() {
        match bytes[i] {
            b'0'..=b'9' => saw_digit = true,
            b'.' if !saw_dot => saw_dot = true,
            _ => return false,
        }
        i += 1;
    }
    saw_digit
}

// ── integer + derivations ────────────────────────────────────────────────────

/// Parse `xs:integer` — arbitrary-precision signed.  Falls back to
/// [`Value::BigInt`] above i128 range.
pub fn parse_integer(s: &str) -> Result<Value, TypeError> {
    parse_integer_inner(s, /*allow_sign=*/ true, /*require_positive=*/ false, None, None)
}

/// Generic ranged-integer parser.  Used by every derivation.
pub fn parse_int_in_range(
    s: &str,
    min: i128,
    max: i128,
    type_name: &'static str,
) -> Result<Value, TypeError> {
    if !is_integer_lexical(s) {
        return Err(TypeError::type_mismatch(
            format!("invalid {type_name}: {s:?}")
        ));
    }
    let n: i128 = s.parse().map_err(|e|
        TypeError::type_mismatch(format!("invalid {type_name} {s:?}: {e}"))
    )?;
    if n < min || n > max {
        return Err(TypeError::type_mismatch(
            format!("{type_name} {n} out of range [{min}, {max}]")
        ));
    }
    Ok(Value::Int(n))
}

/// Same, but the upper bound exceeds i128 range — used for
/// `xs:unsignedLong` (0..=2^64-1).
pub fn parse_unsigned_long(s: &str) -> Result<Value, TypeError> {
    if !is_integer_lexical(s) {
        return Err(TypeError::type_mismatch(
            format!("invalid unsignedLong: {s:?}")
        ));
    }
    let stripped = s.strip_prefix('+').unwrap_or(s);
    if stripped.starts_with('-') {
        return Err(TypeError::type_mismatch(
            format!("unsignedLong cannot be negative: {s:?}")
        ));
    }
    let n: u64 = stripped.parse().map_err(|e|
        TypeError::type_mismatch(format!("invalid unsignedLong {s:?}: {e}"))
    )?;
    Ok(Value::Int(n as i128))
}

fn parse_integer_inner(
    s: &str,
    allow_sign:        bool,
    require_positive:  bool,
    min:               Option<i128>,
    max:               Option<i128>,
) -> Result<Value, TypeError> {
    if !is_integer_lexical(s) {
        return Err(TypeError::type_mismatch(format!("invalid integer: {s:?}")));
    }
    let no_sign = !allow_sign && (s.starts_with('+') || s.starts_with('-'));
    if no_sign {
        return Err(TypeError::type_mismatch("sign not allowed"));
    }
    // Try i128 first; fall through to BigInt only when needed.
    match s.parse::<i128>() {
        Ok(n) => {
            if require_positive && n <= 0 {
                return Err(TypeError::type_mismatch("must be positive"));
            }
            if let Some(lo) = min { if n < lo {
                return Err(TypeError::type_mismatch(format!("{n} < {lo}")));
            } }
            if let Some(hi) = max { if n > hi {
                return Err(TypeError::type_mismatch(format!("{n} > {hi}")));
            } }
            Ok(Value::Int(n))
        }
        Err(_) => {
            // Out of i128 range — build a BigInt.
            let (negative, body) = if let Some(rest) = s.strip_prefix('-') {
                (true, rest.trim_start_matches('0'))
            } else {
                let rest = s.strip_prefix('+').unwrap_or(s);
                (false, rest.trim_start_matches('0'))
            };
            let body = if body.is_empty() { "0" } else { body };
            // Range checks on BigInt are limited in v1: we can detect
            // unsigned/positive-only constraints via the sign and
            // raw-string length.  Reject obvious negatives for
            // require_positive types.
            if require_positive && (negative || body == "0") {
                return Err(TypeError::type_mismatch("must be positive"));
            }
            // Range bounds outside i128 are rare; leave them to PR-time
            // facet checks.  (No standard XSD type beyond the ones we
            // handle above ranges past i128.)
            let _ = (min, max);
            Ok(Value::BigInt(Box::new(BigInt {
                negative,
                digits: body.to_owned(),
            })))
        }
    }
}

fn is_integer_lexical(s: &str) -> bool {
    if s.is_empty() { return false; }
    let bytes = s.as_bytes();
    let mut i = 0;
    if matches!(bytes[0], b'+' | b'-') { i += 1; }
    if i == bytes.len() { return false; }
    bytes[i..].iter().all(|b| b.is_ascii_digit())
}

// Each derivation as a thin wrapper.

pub fn parse_long(s: &str)               -> Result<Value, TypeError> { parse_int_in_range(s, i64::MIN as i128, i64::MAX as i128, "long") }
pub fn parse_int(s: &str)                -> Result<Value, TypeError> { parse_int_in_range(s, i32::MIN as i128, i32::MAX as i128, "int") }
pub fn parse_short(s: &str)              -> Result<Value, TypeError> { parse_int_in_range(s, i16::MIN as i128, i16::MAX as i128, "short") }
pub fn parse_byte(s: &str)               -> Result<Value, TypeError> { parse_int_in_range(s, i8::MIN as i128,  i8::MAX as i128,  "byte") }
pub fn parse_unsigned_int(s: &str)       -> Result<Value, TypeError> { parse_int_in_range(s, 0, u32::MAX as i128, "unsignedInt") }
pub fn parse_unsigned_short(s: &str)     -> Result<Value, TypeError> { parse_int_in_range(s, 0, u16::MAX as i128, "unsignedShort") }
pub fn parse_unsigned_byte(s: &str)      -> Result<Value, TypeError> { parse_int_in_range(s, 0, u8::MAX  as i128, "unsignedByte") }

pub fn parse_non_positive(s: &str)       -> Result<Value, TypeError> {
    let v = parse_integer(s)?;
    let n = match &v { Value::Int(n) => *n, _ => 0 };
    if n > 0 { return Err(TypeError::type_mismatch(format!("nonPositiveInteger > 0: {n}"))); }
    Ok(v)
}
pub fn parse_negative(s: &str)           -> Result<Value, TypeError> {
    let v = parse_integer(s)?;
    let n = match &v { Value::Int(n) => *n, _ => 0 };
    if n >= 0 { return Err(TypeError::type_mismatch(format!("negativeInteger >= 0: {n}"))); }
    Ok(v)
}
pub fn parse_non_negative(s: &str)       -> Result<Value, TypeError> {
    let v = parse_integer(s)?;
    let n = match &v { Value::Int(n) => *n, Value::BigInt(b) if !b.negative => 1, Value::BigInt(_) => -1, _ => 0 };
    if n < 0 { return Err(TypeError::type_mismatch("nonNegativeInteger < 0")); }
    Ok(v)
}
pub fn parse_positive(s: &str)           -> Result<Value, TypeError> {
    let v = parse_integer(s)?;
    let positive = match &v {
        Value::Int(n)    => *n > 0,
        Value::BigInt(b) => !b.negative && b.digits != "0",
        _ => false,
    };
    if !positive { return Err(TypeError::type_mismatch("positiveInteger <= 0")); }
    Ok(v)
}

// ── float / double ───────────────────────────────────────────────────────────

/// XSD float-family parser shared by `xs:float` and `xs:double`.  Accepts
/// the special tokens `INF`, `-INF`, `NaN` (case-sensitive per spec).
fn parse_float_like(s: &str, type_name: &'static str)
    -> Result<f64, TypeError>
{
    if s.is_empty() {
        return Err(TypeError::type_mismatch(format!("empty {type_name}")));
    }
    match s {
        "INF"  => return Ok(f64::INFINITY),
        "-INF" => return Ok(f64::NEG_INFINITY),
        "NaN"  => return Ok(f64::NAN),
        _ => {}
    }
    if !is_float_lexical(s) {
        return Err(TypeError::type_mismatch(format!("invalid {type_name}: {s:?}")));
    }
    s.parse::<f64>().map_err(|e|
        TypeError::type_mismatch(format!("invalid {type_name} {s:?}: {e}"))
    )
}

pub fn parse_float(s: &str) -> Result<Value, TypeError> {
    let n = parse_float_like(s, "float")?;
    Ok(Value::Float(n as f32))
}

pub fn parse_double(s: &str) -> Result<Value, TypeError> {
    let n = parse_float_like(s, "double")?;
    Ok(Value::Double(n))
}

fn is_float_lexical(s: &str) -> bool {
    // Per XSD §3.2.4: optional sign, mantissa (digits with optional .),
    // optional exponent (e or E + optional sign + digits).
    let bytes = s.as_bytes();
    let mut i = 0;
    if matches!(bytes.first(), Some(b'+' | b'-')) { i += 1; }
    let mantissa_start = i;
    let mut saw_digit = false;
    let mut saw_dot   = false;
    while i < bytes.len() {
        match bytes[i] {
            b'0'..=b'9' => { saw_digit = true; i += 1; }
            b'.' if !saw_dot => { saw_dot = true; i += 1; }
            b'e' | b'E' => break,
            _ => return false,
        }
    }
    if !saw_digit || mantissa_start == i { return false; }
    if i == bytes.len() { return true; }
    // Exponent: 'e' or 'E' (already at i), optional sign, digits.
    debug_assert!(matches!(bytes[i], b'e' | b'E'));
    i += 1;
    if i == bytes.len() { return false; }
    if matches!(bytes[i], b'+' | b'-') { i += 1; }
    if i == bytes.len() { return false; }
    bytes[i..].iter().all(|b| b.is_ascii_digit())
}

// ── binary types ─────────────────────────────────────────────────────────────

/// `xs:hexBinary` — pairs of hex digits, case-insensitive.  Empty is OK.
pub fn parse_hex_binary(s: &str) -> Result<Value, TypeError> {
    if s.len() % 2 != 0 {
        return Err(TypeError::type_mismatch("hexBinary length must be even"));
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    let bytes = s.as_bytes();
    for chunk in bytes.chunks(2) {
        let hi = hex_nibble(chunk[0])?;
        let lo = hex_nibble(chunk[1])?;
        out.push((hi << 4) | lo);
    }
    Ok(Value::Bytes(out))
}

fn hex_nibble(b: u8) -> Result<u8, TypeError> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err(TypeError::type_mismatch(format!("invalid hex digit: {:?}", b as char))),
    }
}

/// `xs:base64Binary` — standard alphabet (A-Z, a-z, 0-9, +, /), with `=`
/// padding.  XSD allows whitespace inside the lexical form (handled by
/// the `collapse` whitespace mode at the type-level call site).
pub fn parse_base64_binary(s: &str) -> Result<Value, TypeError> {
    let bytes = s.as_bytes();
    if bytes.len() % 4 != 0 {
        return Err(TypeError::type_mismatch(
            "base64Binary length must be a multiple of 4"
        ));
    }
    if bytes.is_empty() {
        return Ok(Value::Bytes(Vec::new()));
    }

    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    let mut i = 0;
    while i + 4 <= bytes.len() {
        let q = &bytes[i..i + 4];
        // Decode the four sextets.
        let pad_count = q.iter().rev().take_while(|&&b| b == b'=').count();
        if pad_count > 2 {
            return Err(TypeError::type_mismatch("too many '=' padding chars"));
        }
        // Padding may only appear in the final quad.
        if pad_count > 0 && i + 4 != bytes.len() {
            return Err(TypeError::type_mismatch("'=' padding only allowed at end"));
        }
        let s0 = b64_value(q[0])?;
        let s1 = b64_value(q[1])?;
        let s2 = if pad_count >= 2 { 0 } else { b64_value(q[2])? };
        let s3 = if pad_count >= 1 { 0 } else { b64_value(q[3])? };
        out.push((s0 << 2) | (s1 >> 4));
        if pad_count < 2 { out.push((s1 << 4) | (s2 >> 2)); }
        if pad_count < 1 { out.push((s2 << 6) | s3); }
        i += 4;
    }
    Ok(Value::Bytes(out))
}

fn b64_value(b: u8) -> Result<u8, TypeError> {
    match b {
        b'A'..=b'Z' => Ok(b - b'A'),
        b'a'..=b'z' => Ok(b - b'a' + 26),
        b'0'..=b'9' => Ok(b - b'0' + 52),
        b'+'        => Ok(62),
        b'/'        => Ok(63),
        b'='        => Err(TypeError::type_mismatch("misplaced '=' padding")),
        _           => Err(TypeError::type_mismatch(
            format!("invalid base64 char: {:?}", b as char)
        )),
    }
}

// ── anyURI ───────────────────────────────────────────────────────────────────

/// `xs:anyURI` — XSD §3.2.17 says only that escaped values must be valid
/// URIs.  In practice every implementation accepts the union of RFC 3986
/// URIs and IRIs and rejects only obviously broken inputs (whitespace,
/// control chars).  We do the same.  Empty is allowed.
pub fn parse_any_uri(s: &str) -> Result<Value, TypeError> {
    for c in s.chars() {
        if c.is_control() || c == ' ' {
            return Err(TypeError::type_mismatch(
                format!("anyURI cannot contain {c:?}")
            ));
        }
    }
    Ok(Value::Token(s.to_owned()))
}

// ── QName / NOTATION ─────────────────────────────────────────────────────────

/// `xs:QName` — `(prefix:)?local`.  Each part must be an NCName.  The
/// prefix-to-namespace resolution happens at the validation site (it
/// depends on in-scope `xmlns:*` declarations); we only validate the
/// lexical shape here.
pub fn parse_qname(s: &str) -> Result<Value, TypeError> {
    let mut parts = s.splitn(3, ':');
    let p1 = parts.next().unwrap_or("");
    let p2 = parts.next();
    let p3 = parts.next();
    if p3.is_some() {
        return Err(TypeError::type_mismatch(
            format!("QName has more than one ':': {s:?}")
        ));
    }
    match p2 {
        None => {
            // Unprefixed.
            ensure_ncname(p1)?;
        }
        Some(local) => {
            ensure_ncname(p1)?;
            ensure_ncname(local)?;
        }
    }
    Ok(Value::Token(s.to_owned()))
}

/// `xs:NOTATION` — same lexical form as QName.  The schema compiler
/// additionally requires that the QName resolve to a declared
/// `<xs:notation>`; that check happens at validation time.
pub fn parse_notation(s: &str) -> Result<Value, TypeError> {
    parse_qname(s)
}

fn ensure_ncname(s: &str) -> Result<(), TypeError> {
    if s.is_empty() {
        return Err(TypeError::type_mismatch("NCName part cannot be empty"));
    }
    let mut chars = s.chars();
    let first = chars.next().unwrap();
    if !is_xml_name_start_char(first) || first == ':' {
        return Err(TypeError::type_mismatch(
            format!("invalid NCName start: {first:?}")
        ));
    }
    for c in chars {
        if !is_xml_name_char(c) || c == ':' {
            return Err(TypeError::type_mismatch(
                format!("invalid NCName char: {c:?}")
            ));
        }
    }
    Ok(())
}

// Mirrored from types.rs — kept private here for the QName/NOTATION
// validators.  Single source of truth would be nicer but creates a cycle.
fn is_xml_name_start_char(c: char) -> bool {
    matches!(c,
        ':' | 'A'..='Z' | '_' | 'a'..='z'
        | '\u{C0}'..='\u{D6}' | '\u{D8}'..='\u{F6}' | '\u{F8}'..='\u{2FF}'
        | '\u{370}'..='\u{37D}' | '\u{37F}'..='\u{1FFF}'
        | '\u{200C}'..='\u{200D}' | '\u{2070}'..='\u{218F}'
        | '\u{2C00}'..='\u{2FEF}' | '\u{3001}'..='\u{D7FF}'
        | '\u{F900}'..='\u{FDCF}' | '\u{FDF0}'..='\u{FFFD}'
        | '\u{10000}'..='\u{EFFFF}'
    )
}

fn is_xml_name_char(c: char) -> bool {
    is_xml_name_start_char(c)
        || matches!(c,
            '-' | '.' | '0'..='9' | '\u{B7}'
            | '\u{0300}'..='\u{036F}' | '\u{203F}'..='\u{2040}'
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── decimal ──────────────────────────────────────────────────────

    #[test]
    fn decimal_basic() {
        assert!(matches!(parse_decimal("3.14"),    Ok(Value::Decimal(_))));
        assert!(matches!(parse_decimal("-0.001"),  Ok(Value::Decimal(_))));
        assert!(matches!(parse_decimal("100"),     Ok(Value::Decimal(_))));
        assert!(matches!(parse_decimal("+1.0"),    Ok(Value::Decimal(_))));
    }

    #[test]
    fn decimal_rejects_scientific() {
        assert!(parse_decimal("1e2").is_err());
        assert!(parse_decimal("1.5E-3").is_err());
    }

    #[test]
    fn decimal_rejects_garbage() {
        assert!(parse_decimal("").is_err());
        assert!(parse_decimal(".").is_err());
        assert!(parse_decimal("abc").is_err());
        assert!(parse_decimal("1.2.3").is_err());
    }

    // ── integer + derivations ────────────────────────────────────────

    #[test]
    fn integer_in_i128_range() {
        assert!(matches!(parse_integer("0"),        Ok(Value::Int(0))));
        assert!(matches!(parse_integer("-9999"),    Ok(Value::Int(-9999))));
        assert!(matches!(parse_integer("+12345"),   Ok(Value::Int(12345))));
    }

    #[test]
    fn integer_overflows_to_bigint() {
        // Larger than i128::MAX.
        let huge = "999999999999999999999999999999999999999";
        assert!(matches!(parse_integer(huge), Ok(Value::BigInt(_))));
    }

    #[test]
    fn byte_range() {
        assert!(parse_byte("127").is_ok());
        assert!(parse_byte("-128").is_ok());
        assert!(parse_byte("128").is_err());
        assert!(parse_byte("-129").is_err());
    }

    #[test]
    fn unsigned_byte_rejects_negative() {
        assert!(parse_int_in_range("-1", 0, 255, "unsignedByte").is_err());
        assert!(parse_int_in_range("0",  0, 255, "unsignedByte").is_ok());
        assert!(parse_int_in_range("255", 0, 255, "unsignedByte").is_ok());
        assert!(parse_int_in_range("256", 0, 255, "unsignedByte").is_err());
    }

    #[test]
    fn unsigned_long_full_range() {
        assert!(parse_unsigned_long("18446744073709551615").is_ok());
        assert!(parse_unsigned_long("-1").is_err());
    }

    #[test]
    fn positive_integer_excludes_zero() {
        assert!(parse_positive("1").is_ok());
        assert!(parse_positive("0").is_err());
        assert!(parse_positive("-1").is_err());
    }

    #[test]
    fn non_negative_includes_zero() {
        assert!(parse_non_negative("0").is_ok());
        assert!(parse_non_negative("100").is_ok());
        assert!(parse_non_negative("-1").is_err());
    }

    #[test]
    fn negative_integer_excludes_zero() {
        assert!(parse_negative("-1").is_ok());
        assert!(parse_negative("0").is_err());
        assert!(parse_negative("1").is_err());
    }

    // ── float / double ──────────────────────────────────────────────

    #[test]
    fn float_basic() {
        assert!(matches!(parse_float("3.14"),  Ok(Value::Float(_))));
        assert!(matches!(parse_float("-1e10"), Ok(Value::Float(_))));
        assert!(matches!(parse_float("0"),     Ok(Value::Float(_))));
    }

    #[test]
    fn float_special_tokens() {
        match parse_float("INF").unwrap()  { Value::Float(f) => assert!(f.is_infinite() && f.is_sign_positive()), _ => panic!() }
        match parse_float("-INF").unwrap() { Value::Float(f) => assert!(f.is_infinite() && f.is_sign_negative()), _ => panic!() }
        match parse_float("NaN").unwrap()  { Value::Float(f) => assert!(f.is_nan()), _ => panic!() }
    }

    #[test]
    fn float_special_tokens_case_sensitive() {
        // Per spec, "inf" / "nan" are NOT valid.
        assert!(parse_float("inf").is_err());
        assert!(parse_float("nan").is_err());
        assert!(parse_float("Inf").is_err());
    }

    #[test]
    fn double_scientific() {
        match parse_double("1.5E-3").unwrap() { Value::Double(d) => assert!((d - 0.0015).abs() < 1e-9), _ => panic!() }
    }

    #[test]
    fn float_rejects_garbage() {
        assert!(parse_float("").is_err());
        assert!(parse_float("e10").is_err());
        assert!(parse_float("1e").is_err());
        assert!(parse_float("1.2.3").is_err());
    }

    // ── binary ───────────────────────────────────────────────────────

    #[test]
    fn hex_binary_basic() {
        match parse_hex_binary("DEADBEEF").unwrap() {
            Value::Bytes(b) => assert_eq!(b, vec![0xde, 0xad, 0xbe, 0xef]),
            _ => panic!(),
        }
    }

    #[test]
    fn hex_binary_lowercase() {
        match parse_hex_binary("0a1b").unwrap() {
            Value::Bytes(b) => assert_eq!(b, vec![0x0a, 0x1b]),
            _ => panic!(),
        }
    }

    #[test]
    fn hex_binary_empty_ok() {
        // XSD allows empty hexBinary.
        match parse_hex_binary("").unwrap() {
            Value::Bytes(b) => assert!(b.is_empty()),
            _ => panic!(),
        }
    }

    #[test]
    fn hex_binary_rejects_odd_length() {
        assert!(parse_hex_binary("abc").is_err());
    }

    #[test]
    fn hex_binary_rejects_non_hex() {
        assert!(parse_hex_binary("xx").is_err());
    }

    #[test]
    fn base64_binary_basic() {
        match parse_base64_binary("aGVsbG8=").unwrap() {
            Value::Bytes(b) => assert_eq!(b, b"hello"),
            _ => panic!(),
        }
    }

    #[test]
    fn base64_binary_no_padding_when_aligned() {
        match parse_base64_binary("Zm9vYmFy").unwrap() {
            Value::Bytes(b) => assert_eq!(b, b"foobar"),
            _ => panic!(),
        }
    }

    #[test]
    fn base64_binary_two_pad() {
        match parse_base64_binary("Zg==").unwrap() {
            Value::Bytes(b) => assert_eq!(b, b"f"),
            _ => panic!(),
        }
    }

    #[test]
    fn base64_binary_one_pad() {
        match parse_base64_binary("Zm8=").unwrap() {
            Value::Bytes(b) => assert_eq!(b, b"fo"),
            _ => panic!(),
        }
    }

    #[test]
    fn base64_binary_rejects_garbage() {
        assert!(parse_base64_binary("@@@@").is_err());
        assert!(parse_base64_binary("Zg=").is_err()); // wrong pad count
    }

    // ── anyURI / QName / NOTATION ─────────────────────────────────────

    #[test]
    fn any_uri_accepts_common_forms() {
        for s in [
            "http://example.com/",
            "urn:isbn:0451450523",
            "ftp://user:pass@host:21/path",
            "relative/path",
            "#fragment-only",
            "mailto:foo@example.com",
            "",
        ] {
            assert!(parse_any_uri(s).is_ok(), "{s}");
        }
    }

    #[test]
    fn any_uri_rejects_disallowed_chars() {
        // Per RFC 3986 / XSD §3.2.17, control chars and certain ASCII
        // punctuation are rejected.
        assert!(parse_any_uri("with space").is_err());
        assert!(parse_any_uri("ctrl\x01char").is_err());
    }

    #[test]
    fn qname_simple() {
        assert!(parse_qname("name").is_ok());
        assert!(parse_qname("ns:name").is_ok());
    }

    #[test]
    fn qname_rejects_two_colons() {
        assert!(parse_qname("a:b:c").is_err());
    }

    #[test]
    fn qname_rejects_empty_parts() {
        assert!(parse_qname(":name").is_err());
        assert!(parse_qname("name:").is_err());
    }

    #[test]
    fn notation_same_as_qname() {
        assert!(parse_notation("ns:foo").is_ok());
        assert!(parse_notation("a:b:c").is_err());
    }
}
