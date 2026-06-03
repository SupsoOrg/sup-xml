//! EXSLT regular-expression family — http://exslt.org/regular-expressions
//!
//! Three functions:
//!
//! | Function          | Status      | Notes                                          |
//! |-------------------|-------------|------------------------------------------------|
//! | `regexp:test`     | implemented | boolean predicate                              |
//! | `regexp:replace`  | implemented | text rewrite, honours `g` (global) flag         |
//! | `regexp:match`    | implemented | returns a node-set of capture text nodes       |
//!
//! Flags: `i` (case-insensitive), `s` (dotall), `g` (replace-all
//! for `replace`; "all matches as separate text nodes" for `match`).
//! Semantics match libexslt's reference implementation.

use crate::xpath::eval::Value;
use crate::xpath::index::DocIndexLike;
use crate::error::{ErrorDomain, ErrorLevel, XmlError};

use super::Result;

/// Dispatch entry point for the `http://exslt.org/regular-expressions`
/// namespace.  Returns `Some(_)` for recognised function names,
/// `None` to fall through to other dispatchers.
pub fn dispatch<I: DocIndexLike>(
    name: &str,
    args: Vec<Value>,
    idx: &I,
) -> Option<Result<Value>> {
    match name {
        "test"    => Some(regexp_test(&args, idx)),
        "replace" => Some(regexp_replace(&args, idx)),
        "match"   => Some(regexp_match(&args, idx)),
        _ => None,
    }
}

/// `regexp:match(string, pattern[, flags]) → node-set` — return a
/// node-set of text nodes containing the captured substrings.
///
/// Without the `g` flag: one match's captures.  Position 0 is the
/// whole match; positions 1..N are each parenthesised group's text
/// (empty string for groups that didn't participate).  If the
/// pattern doesn't match, the result is an empty node-set.
///
/// With the `g` flag: every non-overlapping match's full match text
/// concatenated in document order (capture groups are dropped — this
/// matches libexslt's interpretation, where `g` shifts the meaning
/// from "destructure one match" to "enumerate all matches").
///
/// An invalid regex returns an empty node-set, mirroring `test` and
/// `replace`.
fn regexp_match<I: DocIndexLike>(args: &[Value], idx: &I) -> Result<Value> {
    if args.len() < 2 || args.len() > 3 {
        return Err(err("regexp:match takes 2 or 3 arguments"));
    }
    let s       = value_as_string(&args[0], idx);
    let pattern = value_as_string(&args[1], idx);
    let flags   = args.get(2).map(|v| value_as_string(v, idx)).unwrap_or_default();
    let re = match build_regex(&pattern, &flags) {
        Ok(r)  => r,
        Err(_) => return Ok(Value::NodeSet(Vec::new())),
    };
    let captures: Vec<String> = if flags.contains('g') {
        re.find_iter(&s).map(|m| m.as_str().to_string()).collect()
    } else {
        match re.captures(&s) {
            Some(caps) => caps.iter()
                .map(|c| c.map(|m| m.as_str().to_string()).unwrap_or_default())
                .collect(),
            None => Vec::new(),
        }
    };
    let ids = idx.allocate_rtf_text_nodes(captures).ok_or_else(|| err(
        "regexp:match: this XPath context does not support RTF allocation",
    ))?;
    Ok(Value::NodeSet(ids))
}

fn err(msg: impl Into<String>) -> XmlError {
    XmlError::new(ErrorDomain::XPath, ErrorLevel::Error, msg.into())
}

/// `regexp:test(string, pattern[, flags]) → boolean` — true iff
/// `pattern` matches anywhere in `string`.  An invalid regex
/// produces `false` (libexslt does the same).
fn regexp_test<I: DocIndexLike>(args: &[Value], idx: &I) -> Result<Value> {
    if args.len() < 2 || args.len() > 3 {
        return Err(err("regexp:test takes 2 or 3 arguments"));
    }
    let s       = value_as_string(&args[0], idx);
    let pattern = value_as_string(&args[1], idx);
    let flags   = args.get(2).map(|v| value_as_string(v, idx)).unwrap_or_default();
    Ok(Value::Boolean(match build_regex(&pattern, &flags) {
        Ok(re) => re.is_match(&s),
        Err(_) => false,
    }))
}

/// `regexp:replace(string, pattern, flags, replacement) → string` —
/// rewrite occurrences of `pattern`.  With the `g` flag every match
/// is rewritten; without, only the first.  Invalid regex returns
/// the original string unchanged.
fn regexp_replace<I: DocIndexLike>(args: &[Value], idx: &I) -> Result<Value> {
    if args.len() != 4 {
        return Err(err("regexp:replace takes 4 arguments"));
    }
    let s       = value_as_string(&args[0], idx);
    let pattern = value_as_string(&args[1], idx);
    let flags   = value_as_string(&args[2], idx);
    let repl    = value_as_string(&args[3], idx);
    Ok(Value::String(match build_regex(&pattern, &flags) {
        Ok(re) => {
            if flags.contains('g') {
                re.replace_all(&s, repl.as_str()).into_owned()
            } else {
                re.replace(&s, repl.as_str()).into_owned()
            }
        }
        Err(_) => s,
    }))
}

/// Compile `pattern` with the EXSLT `i` / `s` flag set baked in as
/// an inline `(?is)`-style group.  Returns the compile error from
/// `regex::Regex::new` on invalid input so the caller can fall back
/// to the EXSLT "invalid regex → no-op" semantics.
fn build_regex(pattern: &str, flags: &str) -> std::result::Result<regex::Regex, regex::Error> {
    let case_i = flags.contains('i');
    let dotall = flags.contains('s');
    if case_i || dotall {
        let mut rebuilt = String::with_capacity(pattern.len() + 5);
        rebuilt.push_str("(?");
        if case_i { rebuilt.push('i'); }
        if dotall { rebuilt.push('s'); }
        rebuilt.push(')');
        rebuilt.push_str(pattern);
        regex::Regex::new(&rebuilt)
    } else {
        regex::Regex::new(pattern)
    }
}

/// Loose `Value` → `String` coercion.  Node-sets collapse to the
/// first node's string-value (XPath §4.2 string()); foreign-node-
/// sets fall back to empty because the EXSLT regex family is
/// always handed pre-stringified data in practice.
fn value_as_string<I: DocIndexLike>(v: &Value, idx: &I) -> String {
    match v {
        Value::String(s)  => s.clone(),
        Value::Number(n)  => format!("{}", n.as_f64()),
        Value::Boolean(b) => if *b { "true".into() } else { "false".into() },
        Value::NodeSet(ns) => ns.first()
            .map(|&id| idx.string_value(id))
            .unwrap_or_default(),
        Value::ForeignNodeSet(_) => String::new(),
        Value::Typed(t)   => t.lexical.clone(),
        Value::Sequence(items) => items.first()
            .map(|v| value_as_string(v, idx))
            .unwrap_or_default(),
        Value::IntRange { lo, .. } => lo.to_string(),
        Value::Map(_) | Value::Array(_) | Value::Function(_) => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{parse_str, ParseOptions, XPathContext};

    fn doc() -> sup_xml_tree::dom::Document {
        parse_str("<r/>", &ParseOptions::default()).unwrap()
    }

    #[test]
    fn test_matches_anywhere() {
        let d = doc();
        let ctx = XPathContext::new(&d);
        let r = dispatch("test",
            vec![Value::String("hello world".into()),
                 Value::String("o w".into())],
            &ctx.index).unwrap().unwrap();
        assert!(matches!(r, Value::Boolean(true)));
    }

    #[test]
    fn test_case_insensitive() {
        let d = doc();
        let ctx = XPathContext::new(&d);
        let r = dispatch("test",
            vec![Value::String("Hello".into()),
                 Value::String("hello".into()),
                 Value::String("i".into())],
            &ctx.index).unwrap().unwrap();
        assert!(matches!(r, Value::Boolean(true)));
    }

    #[test]
    fn test_invalid_regex_is_false() {
        let d = doc();
        let ctx = XPathContext::new(&d);
        let r = dispatch("test",
            vec![Value::String("anything".into()),
                 Value::String("(".into())],
            &ctx.index).unwrap().unwrap();
        assert!(matches!(r, Value::Boolean(false)));
    }

    #[test]
    fn replace_first_match() {
        let d = doc();
        let ctx = XPathContext::new(&d);
        let r = dispatch("replace",
            vec![Value::String("foo foo foo".into()),
                 Value::String("foo".into()),
                 Value::String("".into()),
                 Value::String("bar".into())],
            &ctx.index).unwrap().unwrap();
        match r {
            Value::String(s) => assert_eq!(s, "bar foo foo"),
            _ => panic!(),
        }
    }

    #[test]
    fn replace_global_with_g_flag() {
        let d = doc();
        let ctx = XPathContext::new(&d);
        let r = dispatch("replace",
            vec![Value::String("foo foo foo".into()),
                 Value::String("foo".into()),
                 Value::String("g".into()),
                 Value::String("bar".into())],
            &ctx.index).unwrap().unwrap();
        match r {
            Value::String(s) => assert_eq!(s, "bar bar bar"),
            _ => panic!(),
        }
    }

    #[test]
    fn replace_with_backreference() {
        let d = doc();
        let ctx = XPathContext::new(&d);
        let r = dispatch("replace",
            vec![Value::String("hello".into()),
                 Value::String("(.+)".into()),
                 Value::String("".into()),
                 Value::String("[$1]".into())],
            &ctx.index).unwrap().unwrap();
        match r {
            Value::String(s) => assert_eq!(s, "[hello]"),
            _ => panic!(),
        }
    }

    #[test]
    fn match_without_g_returns_full_match_and_captures() {
        let d = doc();
        let ctx = XPathContext::new(&d);
        let r = dispatch("match",
            vec![Value::String("foo123bar".into()),
                 Value::String(r"([a-z]+)(\d+)".into())],
            &ctx.index).unwrap().unwrap();
        let ns = match r { Value::NodeSet(ns) => ns, _ => panic!() };
        // Three nodes: full match, group 1, group 2.
        assert_eq!(ns.len(), 3);
        let strs: Vec<String> = ns.iter().map(|&id| ctx.index.string_value(id)).collect();
        assert_eq!(strs, vec!["foo123", "foo", "123"]);
    }

    #[test]
    fn match_global_returns_all_matches() {
        let d = doc();
        let ctx = XPathContext::new(&d);
        let r = dispatch("match",
            vec![Value::String("foo1 bar2 baz3".into()),
                 Value::String(r"[a-z]+\d".into()),
                 Value::String("g".into())],
            &ctx.index).unwrap().unwrap();
        let ns = match r { Value::NodeSet(ns) => ns, _ => panic!() };
        let strs: Vec<String> = ns.iter().map(|&id| ctx.index.string_value(id)).collect();
        assert_eq!(strs, vec!["foo1", "bar2", "baz3"]);
    }

    #[test]
    fn match_no_hit_returns_empty_nodeset() {
        let d = doc();
        let ctx = XPathContext::new(&d);
        let r = dispatch("match",
            vec![Value::String("abc".into()),
                 Value::String(r"\d+".into())],
            &ctx.index).unwrap().unwrap();
        let ns = match r { Value::NodeSet(ns) => ns, _ => panic!() };
        assert!(ns.is_empty());
    }

    #[test]
    fn match_via_xpath_expression() {
        use crate::xpath::XPathBindingsBuilder;
        let d = doc();
        let ctx = crate::XPathContext::new(&d);
        let mut bind = XPathBindingsBuilder::new();
        bind.namespace("regexp", "http://exslt.org/regular-expressions");
        let v = ctx.eval_with(r#"regexp:match('foo123', '(\w+?)(\d+)')"#, 0, &bind).unwrap();
        let strs = match v {
            Value::NodeSet(ns) => ns.iter().map(|&id| ctx.index.string_value(id)).collect::<Vec<_>>(),
            _ => panic!("expected nodeset"),
        };
        assert_eq!(strs, vec!["foo123", "foo", "123"]);
    }

    #[test]
    fn match_invalid_regex_is_empty_nodeset() {
        let d = doc();
        let ctx = XPathContext::new(&d);
        let r = dispatch("match",
            vec![Value::String("anything".into()),
                 Value::String("(".into())],
            &ctx.index).unwrap().unwrap();
        assert!(matches!(r, Value::NodeSet(ref ns) if ns.is_empty()));
    }

    #[test]
    fn unknown_name_is_none() {
        let d = doc();
        let ctx = XPathContext::new(&d);
        assert!(dispatch("nonsense",
            vec![Value::String("".into())], &ctx.index).is_none());
    }

    #[test]
    fn test_dotall_flag() {
        let d = doc();
        let ctx = XPathContext::new(&d);
        // Without `s`, `.` doesn't match newline.
        let r = dispatch("test",
            vec![Value::String("a\nb".into()),
                 Value::String("a.b".into())],
            &ctx.index).unwrap().unwrap();
        assert!(matches!(r, Value::Boolean(false)));
        // With `s`, `.` matches newline.
        let r = dispatch("test",
            vec![Value::String("a\nb".into()),
                 Value::String("a.b".into()),
                 Value::String("s".into())],
            &ctx.index).unwrap().unwrap();
        assert!(matches!(r, Value::Boolean(true)));
    }
}
