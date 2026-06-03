//! EXSLT str family — https://exslt.org/str/
//!
//! All functions live in the `http://exslt.org/strings` namespace.
//!
//! Coverage:
//!
//! | Function       | Status      | Notes                                         |
//! |----------------|-------------|-----------------------------------------------|
//! | `concat`       | implemented | nodeset → string                              |
//! | `replace`      | implemented | string-pair or nodeset-pair replacements      |
//! | `padding`      | implemented | repeat-to-length                              |
//! | `align`        | implemented | left / right / center                         |
//! | `tokenize`     | implemented | uses the index's RTF allocator                |
//! | `split`        | implemented | uses the index's RTF allocator                |
//! | `encode-uri`   | implemented | RFC 3986 percent-encoding                     |
//! | `decode-uri`   | implemented | percent-decode                                |
//! | `lower-case`   | implemented | libexslt extension (Unicode case)             |
//! | `upper-case`   | implemented | libexslt extension (Unicode case)             |
//!
//! `tokenize` and `split` produce a node-set of text nodes by
//! allocating into the index's synthetic-text store; the resulting
//! `NodeId`s flow through `for-each`, `value-of`, predicates, and
//! `count()` like any other node-set member.  When invoked under
//! XSLT, the XSLT engine's bindings intercept these calls first
//! and use its own RTF pool — both paths produce equivalent
//! node-sets, just sourced from different stores.

use crate::error::{ErrorDomain, ErrorLevel, XmlError};
use crate::xpath::eval::{Value, value_to_number, value_to_string};
use crate::xpath::index::DocIndexLike;

use super::Result;

fn err(msg: impl Into<String>) -> XmlError {
    XmlError::new(ErrorDomain::XPath, ErrorLevel::Error, msg)
}

pub fn dispatch<I: DocIndexLike>(
    name: &str, args: Vec<Value>, idx: &I,
) -> Option<Result<Value>> {
    let r: Result<Value> = match name {
        "concat"     => concat_fn(&args, idx),
        "replace"    => replace_fn(&args, idx),
        "padding"    => padding_fn(&args, idx),
        "align"      => align_fn(&args, idx),
        "encode-uri" => encode_uri_fn(&args, idx),
        "decode-uri" => decode_uri_fn(&args, idx),
        "lower-case" => lower_upper_fn(&args, idx, |c| c.to_lowercase().collect::<String>()),
        "upper-case" => lower_upper_fn(&args, idx, |c| c.to_uppercase().collect::<String>()),
        "tokenize"   => tokenize_or_split_fn(name, &args, idx),
        "split"      => tokenize_or_split_fn(name, &args, idx),
        _ => return None,
    };
    Some(r)
}

/// `str:tokenize(string, delim?)` / `str:split(string, sep?)`.
/// Both yield a node-set of text nodes — one per token / fragment.
/// Semantics (https://exslt.org/str/):
///
/// * `tokenize`: split on *any* character in `delim`; default delim
///   is `"\t\r\n "` (whitespace).  Empty `delim` returns one node
///   per character.  Empty fragments between adjacent delimiters
///   are dropped.
/// * `split`: split on the literal `sep` string; default sep is a
///   single space.  Empty `sep` returns one node per character.
///   Empty fragments are kept (libexslt does likewise).
fn tokenize_or_split_fn<I: DocIndexLike>(
    fn_name: &str, args: &[Value], idx: &I,
) -> Result<Value> {
    if args.is_empty() || args.len() > 2 {
        return Err(err(format!("str:{fn_name} takes 1 or 2 arguments")));
    }
    let s = value_to_string(&args[0], idx);
    let sep_raw = args.get(1).map(|v| value_to_string(v, idx));
    let tokens: Vec<String> = match fn_name {
        "tokenize" => {
            let delim: &str = sep_raw.as_deref().unwrap_or("\t\r\n ");
            if delim.is_empty() {
                s.chars().map(|c| c.to_string()).collect()
            } else {
                s.split(|c: char| delim.contains(c))
                    .filter(|t| !t.is_empty())
                    .map(|t| t.to_string())
                    .collect()
            }
        }
        "split" => {
            let sep: &str = sep_raw.as_deref().unwrap_or(" ");
            if sep.is_empty() {
                s.chars().map(|c| c.to_string()).collect()
            } else {
                s.split(sep).map(|t| t.to_string()).collect()
            }
        }
        _ => unreachable!(),
    };
    let ids = idx.allocate_rtf_text_nodes(tokens).ok_or_else(|| err(format!(
        "str:{fn_name}: this XPath context does not support RTF allocation"
    )))?;
    Ok(Value::NodeSet(ids))
}

/// `str:encode-uri(uri-part, escape-reserved?, encoding?)` — RFC 3986
/// percent-encoding.  When `escape-reserved` is false (the default)
/// only non-ASCII / unsafe characters are escaped, mirroring
/// JavaScript's `encodeURI`.  When true, reserved gen-delims and
/// sub-delims (`:/?#[]@!$&'()*+,;=`) are also escaped, mirroring
/// `encodeURIComponent`.  The `encoding` argument is parsed for
/// compatibility (EXSLT spec accepts it) but only `UTF-8` is
/// supported — anything else returns the input unchanged with no
/// error, same as libexslt.
fn encode_uri_fn<I: DocIndexLike>(args: &[Value], idx: &I) -> Result<Value> {
    if args.is_empty() || args.len() > 3 {
        return Err(err("str:encode-uri takes 1, 2, or 3 arguments"));
    }
    let s = value_to_string(&args[0], idx);
    let escape_reserved = match args.get(1) {
        Some(Value::Boolean(b)) => *b,
        Some(v) => !value_to_string(v, idx).is_empty()
                    && value_to_string(v, idx) != "false",
        None => false,
    };
    let encoding = args.get(2).map(|v| value_to_string(v, idx))
        .unwrap_or_else(|| "UTF-8".into());
    if !encoding.eq_ignore_ascii_case("UTF-8") && !encoding.is_empty() {
        return Ok(Value::String(s));
    }
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        let safe_unreserved = matches!(b,
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9'
            | b'-' | b'_' | b'.' | b'~'
        );
        // Reserved set per RFC 3986 §2.2.  Preserved verbatim
        // unless escape_reserved is on.
        let reserved = matches!(b,
            b':' | b'/' | b'?' | b'#' | b'[' | b']' | b'@'
            | b'!' | b'$' | b'&' | b'\'' | b'(' | b')'
            | b'*' | b'+' | b',' | b';' | b'='
        );
        if safe_unreserved || (!escape_reserved && reserved) {
            out.push(b as char);
        } else {
            out.push('%');
            out.push_str(&format!("{b:02X}"));
        }
    }
    Ok(Value::String(out))
}

/// `str:decode-uri(uri-part, encoding?)` — RFC 3986 percent-decoding.
/// `%XX` sequences become single bytes; invalid sequences (bad hex)
/// are passed through unchanged.  Only UTF-8 is supported; other
/// encodings return the input unchanged.
fn decode_uri_fn<I: DocIndexLike>(args: &[Value], idx: &I) -> Result<Value> {
    if args.is_empty() || args.len() > 2 {
        return Err(err("str:decode-uri takes 1 or 2 arguments"));
    }
    let s = value_to_string(&args[0], idx);
    let encoding = args.get(1).map(|v| value_to_string(v, idx))
        .unwrap_or_else(|| "UTF-8".into());
    if !encoding.eq_ignore_ascii_case("UTF-8") && !encoding.is_empty() {
        return Ok(Value::String(s));
    }
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = &s[i + 1..i + 3];
            if let Ok(b) = u8::from_str_radix(hex, 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    // Invalid UTF-8 after decode → pass-through with replacement
    // chars; matches libexslt's "lossy on bad input" stance.
    Ok(Value::String(String::from_utf8_lossy(&out).into_owned()))
}

/// `str:lower-case(s)` / `str:upper-case(s)` — Unicode case
/// conversion.  EXSLT's spec uses C locale, but for real-world
/// stylesheets the Unicode rules are what callers expect.
fn lower_upper_fn<I: DocIndexLike>(
    args: &[Value], idx: &I,
    mapper: impl Fn(char) -> String,
) -> Result<Value> {
    if args.len() != 1 {
        return Err(err("str:lower-case / str:upper-case takes 1 argument"));
    }
    let s = value_to_string(&args[0], idx);
    Ok(Value::String(s.chars().map(mapper).collect()))
}

// ── concat ────────────────────────────────────────────────────────

/// `str:concat(nodeset)` — concatenate the string-values of every
/// node in `nodeset` in document order.  Distinct from XPath's
/// built-in `concat(s1, s2, …)` which takes 2+ scalar args.
fn concat_fn<I: DocIndexLike>(args: &[Value], idx: &I) -> Result<Value> {
    if args.len() != 1 {
        return Err(err("str:concat takes a single nodeset argument"));
    }
    let ns = match &args[0] {
        Value::NodeSet(ns) => ns,
        // Spec: non-nodeset arg → operate on its string value
        // (libexslt behaviour — easier to use from outside XSLT).
        other => return Ok(Value::String(value_to_string(other, idx))),
    };
    let mut out = String::new();
    for &id in ns {
        out.push_str(&idx.string_value(id));
    }
    Ok(Value::String(out))
}

// ── replace ───────────────────────────────────────────────────────

/// `str:replace(string, search, replace)` — every occurrence of
/// `search` in `string` is replaced by `replace`.
///
/// The spec allows `search` and `replace` to be nodesets,
/// providing N parallel substitution pairs: position k of `search`
/// pairs with position k of `replace`.  Substitutions are applied
/// in document order, left-to-right, non-overlapping.  Items in
/// `search` beyond `replace`'s length delete the matched text.
fn replace_fn<I: DocIndexLike>(args: &[Value], idx: &I) -> Result<Value> {
    if args.len() != 3 {
        return Err(err("str:replace takes 3 arguments"));
    }
    let s = value_to_string(&args[0], idx);
    let searches  = into_str_list(&args[1], idx);
    let replaces  = into_str_list(&args[2], idx);

    if searches.is_empty() {
        return Ok(Value::String(s));
    }

    // Single-pass replace with leftmost-longest match precedence
    // when multiple search terms could apply at the same position
    // (mirrors libexslt's behaviour).
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    'outer: while i < bytes.len() {
        // Try each search term in order; first hit wins (the spec
        // says "in document order" — same thing here since
        // searches comes from the nodeset in document order).
        for (k, needle) in searches.iter().enumerate() {
            if needle.is_empty() { continue; }
            if s[i..].starts_with(needle.as_str()) {
                if let Some(rep) = replaces.get(k) {
                    out.push_str(rep);
                }
                // searches beyond replaces' length: delete.
                i += needle.len();
                continue 'outer;
            }
        }
        // No match — copy one char (handle UTF-8 boundary).
        let c = s[i..].chars().next().unwrap();
        out.push(c);
        i += c.len_utf8();
    }
    Ok(Value::String(out))
}

/// Coerce a value to a list of strings.  Nodesets → one per node's
/// string value (document order).  Anything else → a single-element
/// list holding its string value.
fn into_str_list<I: DocIndexLike>(v: &Value, idx: &I) -> Vec<String> {
    match v {
        Value::NodeSet(ns) => ns.iter().map(|&id| idx.string_value(id)).collect(),
        other => vec![value_to_string(other, idx)],
    }
}

// ── padding ───────────────────────────────────────────────────────

/// `str:padding(length, chars?)` — returns a string of `length`
/// characters built by repeating `chars` (default `" "`), truncated
/// to exactly `length`.
fn padding_fn<I: DocIndexLike>(args: &[Value], idx: &I) -> Result<Value> {
    if args.is_empty() || args.len() > 2 {
        return Err(err("str:padding takes 1 or 2 arguments"));
    }
    let length = value_to_number(&args[0], idx);
    if !length.is_finite() || length < 0.0 {
        return Ok(Value::String(String::new()));
    }
    let length = length as usize;
    let pad = if args.len() == 2 {
        value_to_string(&args[1], idx)
    } else {
        " ".to_string()
    };
    if pad.is_empty() {
        // libexslt: empty pad → length spaces, matching its
        // "default to space" fallback inside the loop.
        return Ok(Value::String(" ".repeat(length)));
    }
    let mut out = String::with_capacity(length * 2);
    while out.chars().count() < length {
        out.push_str(&pad);
    }
    // Truncate to exactly `length` chars.
    let truncated: String = out.chars().take(length).collect();
    Ok(Value::String(truncated))
}

// ── align ─────────────────────────────────────────────────────────

/// `str:align(target, padding, alignment?)` — return `target`
/// shifted into a slot whose length and pad chars come from
/// `padding`.  `alignment` is one of `"left"` (default), `"right"`,
/// `"center"`.
fn align_fn<I: DocIndexLike>(args: &[Value], idx: &I) -> Result<Value> {
    if args.len() < 2 || args.len() > 3 {
        return Err(err("str:align takes 2 or 3 arguments"));
    }
    let target  = value_to_string(&args[0], idx);
    let padding = value_to_string(&args[1], idx);
    let align   = args.get(2).map(|v| value_to_string(v, idx))
        .unwrap_or_else(|| "left".to_string());

    let pad_chars: Vec<char> = padding.chars().collect();
    let target_chars: Vec<char> = target.chars().collect();
    let n = pad_chars.len();

    if target_chars.len() >= n {
        // Target already fills (or overflows) the slot — truncate.
        return Ok(Value::String(target_chars.into_iter().take(n).collect()));
    }

    let result: String = match align.as_str() {
        "right" => {
            let lead = n - target_chars.len();
            pad_chars[..lead].iter().chain(target_chars.iter()).collect()
        }
        "center" => {
            let space = n - target_chars.len();
            let lead  = space / 2;
            let trail_start = lead + target_chars.len();
            pad_chars[..lead].iter()
                .chain(target_chars.iter())
                .chain(pad_chars[trail_start..].iter())
                .collect()
        }
        _ /* "left" or anything else */ => {
            target_chars.iter()
                .chain(pad_chars[target_chars.len()..].iter())
                .collect()
        }
    };
    Ok(Value::String(result))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xpath::eval::Numeric;
    use crate::xpath::XPathContext;
    use crate::{parse_str, ParseOptions};

    fn tiny() -> sup_xml_tree::dom::Document {
        parse_str("<r/>", &ParseOptions::default()).unwrap()
    }
    fn s(v: &Value) -> String {
        if let Value::String(s) = v { s.clone() } else { panic!("expected string, got {v:?}") }
    }

    #[test]
    fn concat_over_nodeset() {
        let doc = parse_str(
            "<r><i>a</i><i>b</i><i>c</i></r>",
            &ParseOptions::default(),
        ).unwrap();
        let ctx = XPathContext::new(&doc);
        let ns = ctx.eval("/r/i").unwrap();
        let v = dispatch("concat", vec![ns], &ctx.index).unwrap().unwrap();
        assert_eq!(s(&v), "abc");
    }

    #[test]
    fn replace_single_pair() {
        let doc = tiny();
        let ctx = XPathContext::new(&doc);
        let v = dispatch("replace",
            vec![Value::String("hello world".into()),
                 Value::String("world".into()),
                 Value::String("there".into())],
            &ctx.index).unwrap().unwrap();
        assert_eq!(s(&v), "hello there");
    }

    #[test]
    fn replace_no_match_returns_input() {
        let doc = tiny();
        let ctx = XPathContext::new(&doc);
        let v = dispatch("replace",
            vec![Value::String("abc".into()),
                 Value::String("z".into()),
                 Value::String("Z".into())],
            &ctx.index).unwrap().unwrap();
        assert_eq!(s(&v), "abc");
    }

    #[test]
    fn replace_handles_overlapping_matches() {
        // "aaaa" with search "aa" → "bb" should be "bb"+"bb" = "bbbb",
        // not "bba" (avoiding overlap).
        let doc = tiny();
        let ctx = XPathContext::new(&doc);
        let v = dispatch("replace",
            vec![Value::String("aaaa".into()),
                 Value::String("aa".into()),
                 Value::String("bb".into())],
            &ctx.index).unwrap().unwrap();
        assert_eq!(s(&v), "bbbb");
    }

    #[test]
    fn padding_default_space() {
        let doc = tiny();
        let ctx = XPathContext::new(&doc);
        let v = dispatch("padding",
            vec![Value::Number(Numeric::Double(5.0))], &ctx.index).unwrap().unwrap();
        assert_eq!(s(&v), "     ");
    }

    #[test]
    fn padding_repeats_then_truncates() {
        let doc = tiny();
        let ctx = XPathContext::new(&doc);
        let v = dispatch("padding",
            vec![Value::Number(Numeric::Double(7.0)), Value::String("ab".into())],
            &ctx.index).unwrap().unwrap();
        assert_eq!(s(&v), "abababa");
    }

    #[test]
    fn align_left_default() {
        let doc = tiny();
        let ctx = XPathContext::new(&doc);
        let v = dispatch("align",
            vec![Value::String("hi".into()),
                 Value::String("....".into())],
            &ctx.index).unwrap().unwrap();
        assert_eq!(s(&v), "hi..");
    }

    #[test]
    fn align_right() {
        let doc = tiny();
        let ctx = XPathContext::new(&doc);
        let v = dispatch("align",
            vec![Value::String("hi".into()),
                 Value::String("....".into()),
                 Value::String("right".into())],
            &ctx.index).unwrap().unwrap();
        assert_eq!(s(&v), "..hi");
    }

    #[test]
    fn align_center() {
        let doc = tiny();
        let ctx = XPathContext::new(&doc);
        let v = dispatch("align",
            vec![Value::String("hi".into()),
                 Value::String("......".into()),
                 Value::String("center".into())],
            &ctx.index).unwrap().unwrap();
        // 6-char slot, "hi" centred: 2 lead, then "hi", then 2 trail.
        assert_eq!(s(&v), "..hi..");
    }

    #[test]
    fn align_truncates_when_target_overflows() {
        let doc = tiny();
        let ctx = XPathContext::new(&doc);
        let v = dispatch("align",
            vec![Value::String("longstring".into()),
                 Value::String("---".into())],
            &ctx.index).unwrap().unwrap();
        assert_eq!(s(&v), "lon");
    }

    #[test]
    fn tokenize_returns_text_nodeset() {
        let doc = tiny();
        let ctx = XPathContext::new(&doc);
        let r = dispatch("tokenize",
            vec![Value::String("a,b,c".into()), Value::String(",".into())],
            &ctx.index).unwrap().unwrap();
        let ns = match r { Value::NodeSet(ns) => ns, _ => panic!("expected nodeset") };
        assert_eq!(ns.len(), 3);
        let strs: Vec<String> = ns.iter().map(|&id| ctx.index.string_value(id)).collect();
        assert_eq!(strs, vec!["a", "b", "c"]);
    }

    #[test]
    fn tokenize_drops_empty_segments_between_adjacent_delims() {
        let doc = tiny();
        let ctx = XPathContext::new(&doc);
        // "a,,b" with delim "," → ["a", "b"] (empty middle segment dropped).
        let r = dispatch("tokenize",
            vec![Value::String("a,,b".into()), Value::String(",".into())],
            &ctx.index).unwrap().unwrap();
        let ns = match r { Value::NodeSet(ns) => ns, _ => panic!() };
        let strs: Vec<String> = ns.iter().map(|&id| ctx.index.string_value(id)).collect();
        assert_eq!(strs, vec!["a", "b"]);
    }

    #[test]
    fn tokenize_default_delim_is_whitespace() {
        let doc = tiny();
        let ctx = XPathContext::new(&doc);
        let r = dispatch("tokenize",
            vec![Value::String("foo  bar\tbaz".into())], &ctx.index).unwrap().unwrap();
        let ns = match r { Value::NodeSet(ns) => ns, _ => panic!() };
        let strs: Vec<String> = ns.iter().map(|&id| ctx.index.string_value(id)).collect();
        assert_eq!(strs, vec!["foo", "bar", "baz"]);
    }

    #[test]
    fn tokenize_via_xpath_expression_count_and_strings() {
        // Full XPath flow: dispatcher reached via str: prefix in the
        // expression, result is a node-set the rest of XPath can
        // count, sort, predicate, and string-coerce just like any
        // other node-set.
        use crate::xpath::XPathBindingsBuilder;
        let doc = tiny();
        let ctx = XPathContext::new(&doc);
        let mut bind = XPathBindingsBuilder::new();
        bind.namespace("str", "http://exslt.org/strings");
        let v = ctx.eval_with("count(str:tokenize('a,b,c,d', ','))", 0, &bind).unwrap();
        assert_eq!(crate::xpath::eval::value_to_number(&v, &ctx.index), 4.0);
        let v = ctx.eval_with("str:tokenize('foo bar baz')", 0, &bind).unwrap();
        let strs = match v {
            Value::NodeSet(ns) => ns.iter().map(|&id| ctx.index.string_value(id)).collect::<Vec<_>>(),
            _ => panic!("expected nodeset"),
        };
        assert_eq!(strs, vec!["foo", "bar", "baz"]);
    }

    #[test]
    fn split_preserves_empty_segments() {
        let doc = tiny();
        let ctx = XPathContext::new(&doc);
        // "a,,b" with sep "," → ["a", "", "b"] (empty kept — split, not tokenize).
        let r = dispatch("split",
            vec![Value::String("a,,b".into()), Value::String(",".into())],
            &ctx.index).unwrap().unwrap();
        let ns = match r { Value::NodeSet(ns) => ns, _ => panic!() };
        let strs: Vec<String> = ns.iter().map(|&id| ctx.index.string_value(id)).collect();
        assert_eq!(strs, vec!["a", "", "b"]);
    }

    #[test]
    fn unknown_function_returns_none() {
        let doc = tiny();
        let ctx = XPathContext::new(&doc);
        assert!(dispatch("nonsense", vec![], &ctx.index).is_none());
    }

    #[test]
    fn encode_uri_default_preserves_reserved() {
        let doc = tiny();
        let ctx = XPathContext::new(&doc);
        let r = dispatch("encode-uri",
            vec![Value::String("https://example.com/a b?c=1".into())],
            &ctx.index).unwrap().unwrap();
        match r {
            // Space → %20; reserved chars (`:`, `/`, `?`, `=`) untouched
            Value::String(s) => assert_eq!(s, "https://example.com/a%20b?c=1"),
            _ => panic!(),
        }
    }

    #[test]
    fn encode_uri_with_reserved_flag_escapes_all() {
        let doc = tiny();
        let ctx = XPathContext::new(&doc);
        let r = dispatch("encode-uri",
            vec![Value::String("a/b?c=1".into()), Value::Boolean(true)],
            &ctx.index).unwrap().unwrap();
        match r {
            Value::String(s) => assert_eq!(s, "a%2Fb%3Fc%3D1"),
            _ => panic!(),
        }
    }

    #[test]
    fn decode_uri_roundtrips_encode() {
        let doc = tiny();
        let ctx = XPathContext::new(&doc);
        let r = dispatch("decode-uri",
            vec![Value::String("a%20b%2Fc".into())], &ctx.index).unwrap().unwrap();
        match r {
            Value::String(s) => assert_eq!(s, "a b/c"),
            _ => panic!(),
        }
    }

    #[test]
    fn lower_upper_case_unicode() {
        let doc = tiny();
        let ctx = XPathContext::new(&doc);
        let r = dispatch("lower-case",
            vec![Value::String("HëLLo WÖRLD".into())], &ctx.index).unwrap().unwrap();
        match r { Value::String(s) => assert_eq!(s, "hëllo wörld"), _ => panic!() }
        let r = dispatch("upper-case",
            vec![Value::String("straße".into())], &ctx.index).unwrap().unwrap();
        // ß uppercases to SS per Unicode case rules.
        match r { Value::String(s) => assert_eq!(s, "STRASSE"), _ => panic!() }
    }
}
