//! Identity constraints — `<xs:key>` / `<xs:keyref>` / `<xs:unique>`.
//!
//! XSD identity constraints scope a uniqueness or foreign-key check to
//! a subtree.  Each constraint has:
//!
//! * a **selector** XPath that picks the elements the constraint applies
//!   to (relative to the declaring element);
//! * one or more **field** XPaths that extract the key components from
//!   each selected element;
//! * for `xs:keyref`, a `refer` attribute naming a key/unique constraint
//!   whose value-set this one's values must be members of.
//!
//! XSD restricts the XPath syntax for selectors and fields (XSD §3.11.6) —
//! a small subset that we parse with a hand-written micro-parser here
//! rather than by going through the general [`crate::xpath`] engine.
//! That keeps the schema-compile path lean and makes the structural
//! checks unambiguous.
//!
//! ## Scope semantics
//!
//! A constraint declared on `<element foo>` scopes to the subtree
//! rooted at each instance of `foo`.  Within that subtree:
//!
//! * `xs:key` requires every selector-matched element to produce a
//!   unique field-tuple.  A missing field is an error.
//! * `xs:unique` is the same, but missing fields are tolerated.
//! * `xs:keyref` checks that each matched field-tuple appears in the
//!   value-set of the referenced key/unique within the *same* scope or
//!   an enclosing one.  Forward references within a scope are fine
//!   (we resolve at scope-close time).

use std::sync::Arc;

use super::schema::QName;

// ── data structures ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConstraintKind {
    /// `xs:key` — unique, all fields required.
    Key,
    /// `xs:unique` — unique, missing fields allowed.
    Unique,
    /// `xs:keyref` — each tuple must match a key/unique tuple in scope.
    KeyRef,
}

#[derive(Debug, Clone)]
pub struct IdentityConstraint {
    pub name:     QName,
    pub kind:     ConstraintKind,
    pub selector: SelectorPath,
    pub fields:   Vec<FieldPath>,
    /// For `xs:keyref`: the referenced key/unique constraint name.
    pub refer:    Option<QName>,
}

/// Selector path — picks elements relative to the constraint-declaring
/// element.  Per XSD §3.11.6 the syntax is:
///
/// ```text
/// Selector = Path ('|' Path)*
/// Path     = ('.//')? Step ('/' Step)*
/// Step     = '.' | NameTest | ('child::' NameTest)
/// NameTest = QName | '*' | NCName ':*'
/// ```
///
/// We model the union (`|`) as a `Vec<PathExpr>` and each path as a
/// list of [`PathStep`]s with an optional descendant-or-self prefix.
#[derive(Debug, Clone)]
pub struct SelectorPath {
    pub paths: Vec<PathExpr>,
}

#[derive(Debug, Clone)]
pub struct FieldPath {
    pub paths: Vec<PathExpr>,
    /// True iff *every* alternative path ends in an attribute step.
    /// Cached on parse for the field evaluator.
    pub all_attribute: bool,
}

#[derive(Debug, Clone)]
pub struct PathExpr {
    /// True iff the path starts with `.//` (descendant axis).  Without
    /// it, the path is anchored at the constraint-declaring element.
    pub descendant: bool,
    pub steps:      Vec<PathStep>,
}

#[derive(Debug, Clone)]
pub enum PathStep {
    /// `child::NameTest` (or unprefixed shorthand).
    Child(NameTest),
    /// `attribute::NameTest` or `@NameTest` — only legal as the last
    /// step of a *field* path.
    Attribute(NameTest),
}

#[derive(Debug, Clone)]
pub enum NameTest {
    /// `*` — any element / any attribute.
    Any,
    /// Specific name (namespace resolved at compile time using the
    /// schema's prefix bindings).
    Name(QName),
    /// `prefix:*` — any name in the given namespace.
    AnyInNs(Arc<str>),
}

// ── micro-parser for the XSD XPath subset ────────────────────────────────────

/// Parse a `<xs:selector xpath="…">` value.
pub fn parse_selector(
    xpath: &str,
    resolve_prefix: &dyn Fn(&str) -> Option<String>,
) -> Result<SelectorPath, String> {
    let mut paths = Vec::new();
    for part in xpath.split('|') {
        paths.push(parse_path(part.trim(), /*allow_attribute_tail=*/ false, resolve_prefix)?);
    }
    if paths.is_empty() {
        return Err("empty selector".to_string());
    }
    Ok(SelectorPath { paths })
}

/// Parse a `<xs:field xpath="…">` value.  Allows a trailing `@name`
/// step that selectors don't.
pub fn parse_field(
    xpath: &str,
    resolve_prefix: &dyn Fn(&str) -> Option<String>,
) -> Result<FieldPath, String> {
    let mut paths = Vec::new();
    for part in xpath.split('|') {
        paths.push(parse_path(part.trim(), /*allow_attribute_tail=*/ true, resolve_prefix)?);
    }
    if paths.is_empty() {
        return Err("empty field".to_string());
    }
    let all_attribute = paths.iter().all(|p|
        matches!(p.steps.last(), Some(PathStep::Attribute(_)))
    );
    Ok(FieldPath { paths, all_attribute })
}

fn parse_path(
    raw: &str,
    allow_attribute_tail: bool,
    resolve_prefix: &dyn Fn(&str) -> Option<String>,
) -> Result<PathExpr, String> {
    // XPath 1.0 §3.7 — whitespace is insignificant between tokens.
    // Tighten the path to a canonical form so `. //.` and `.//.` parse
    // identically.  We squeeze spaces around the structural separators
    // (`/`, `//`, `::`, `@`) and the `.` self-step.
    let trimmed = raw.trim();
    let normalized = strip_xpath_whitespace(trimmed);
    let raw = normalized.as_str();
    let (descendant, body) = if let Some(rest) = raw.strip_prefix(".//") {
        (true, rest)
    } else if let Some(rest) = raw.strip_prefix("./") {
        (false, rest)
    } else if raw == "." {
        return Ok(PathExpr { descendant: false, steps: Vec::new() });
    } else {
        (false, raw)
    };

    let mut steps = Vec::new();
    let parts: Vec<&str> = body.split('/').collect();
    for (i, part) in parts.iter().enumerate() {
        // XPath 1.0 §3.1 — the axis separator `::` and the `@`
        // abbreviation may be surrounded by whitespace. Strip
        // it before applying our prefix-stripping logic so that
        // `child :: imp:iid` parses the same as `child::imp:iid`.
        let normalized = part.trim()
            .replace(" :: ", "::")
            .replace(":: ", "::")
            .replace(" ::", "::")
            .replace("@ ", "@");
        let p = normalized.trim();
        if p.is_empty() {
            return Err(format!("empty step in path {raw:?}"));
        }
        // `.` is a no-op step (current node); the XSD identity-
        // constraint subset (§3.11.6) allows it anywhere in a path —
        // including as the final step of a field path, even though
        // that selects the current element rather than a downstream
        // value.  XSTS accepts these so we mirror the verdict.
        if p == "." { continue; }
        let is_last = i == parts.len() - 1;

        let step = if let Some(rest) = p.strip_prefix("attribute::") {
            if !allow_attribute_tail || !is_last {
                return Err(format!(
                    "attribute step not allowed here: {raw:?}"
                ));
            }
            PathStep::Attribute(parse_name_test(rest, resolve_prefix)?)
        } else if let Some(rest) = p.strip_prefix('@') {
            if !allow_attribute_tail || !is_last {
                return Err(format!(
                    "attribute step not allowed here: {raw:?}"
                ));
            }
            PathStep::Attribute(parse_name_test(rest, resolve_prefix)?)
        } else {
            let bare = p.strip_prefix("child::").unwrap_or(p);
            PathStep::Child(parse_name_test(bare, resolve_prefix)?)
        };
        steps.push(step);
    }
    if steps.is_empty() && !descendant {
        return Err(format!("path has no steps: {raw:?}"));
    }
    Ok(PathExpr { descendant, steps })
}

fn parse_name_test(
    s: &str,
    resolve_prefix: &dyn Fn(&str) -> Option<String>,
) -> Result<NameTest, String> {
    if s == "*" {
        return Ok(NameTest::Any);
    }
    if s.is_empty() {
        return Err("empty name in identity-constraint XPath".to_string());
    }
    if let Some((prefix, local)) = s.split_once(':') {
        ensure_ncname(prefix)?;
        let ns = resolve_prefix(prefix).ok_or_else(||
            format!("undeclared namespace prefix in identity-constraint XPath: {prefix:?}"))?;
        if local == "*" {
            return Ok(NameTest::AnyInNs(Arc::from(ns)));
        }
        ensure_ncname(local)?;
        Ok(NameTest::Name(QName::new(Some(&ns), local)))
    } else {
        // Unprefixed — XSD says no default namespace lookup applies in
        // identity-constraint XPaths (per the spec, unprefixed names
        // are in no namespace).  Some implementations (libxml2) instead
        // use the schema's targetNamespace as the default.  We follow
        // the stricter "no default namespace" reading and document it.
        ensure_ncname(s)?;
        Ok(NameTest::Name(QName::new(None, s)))
    }
}

/// Reject anything that isn't a syntactically valid `NCName` per XML
/// Names §3.  Predicates (`foo[1]`), function calls (`document('')`),
/// quoted strings, and bare `@` (with no local name to follow) all
/// fall out of this check because their characters aren't legal in an
/// XML name — the XSD identity-constraint XPath subset (§3.11.6)
/// rejects them by construction.
fn ensure_ncname(s: &str) -> Result<(), String> {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return Err("empty NCName in identity-constraint XPath".to_string());
    };
    if !is_name_start(first) || first == ':' {
        return Err(format!("invalid NCName start char {first:?} in identity-constraint XPath"));
    }
    for c in chars {
        if !is_name_char(c) || c == ':' {
            return Err(format!("invalid NCName char {c:?} in identity-constraint XPath"));
        }
    }
    Ok(())
}

fn is_name_start(c: char) -> bool {
    matches!(c,
        'A'..='Z' | '_' | 'a'..='z'
        | '\u{C0}'..='\u{D6}' | '\u{D8}'..='\u{F6}' | '\u{F8}'..='\u{2FF}'
        | '\u{370}'..='\u{37D}' | '\u{37F}'..='\u{1FFF}'
        | '\u{200C}'..='\u{200D}' | '\u{2070}'..='\u{218F}'
        | '\u{2C00}'..='\u{2FEF}' | '\u{3001}'..='\u{D7FF}'
        | '\u{F900}'..='\u{FDCF}' | '\u{FDF0}'..='\u{FFFD}'
        | '\u{10000}'..='\u{EFFFF}'
    )
}

fn is_name_char(c: char) -> bool {
    is_name_start(c)
        || matches!(c,
            '-' | '.' | '0'..='9' | '\u{B7}'
            | '\u{0300}'..='\u{036F}' | '\u{203F}'..='\u{2040}'
        )
}

/// Squeeze whitespace around the structural punctuation an XSD
/// identity-constraint XPath actually uses (`/`, `::`, `@`).  XPath
/// 1.0 §3.7 lets whitespace appear between tokens; we normalise into
/// the no-space form so the downstream splitter doesn't have to.
fn strip_xpath_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_was_struct = false;
    for tok in s.split_whitespace() {
        if !out.is_empty() && !prev_was_struct && !tok.starts_with('/') {
            out.push(' ');
        }
        out.push_str(tok);
        prev_was_struct = tok.ends_with('/');
    }
    // Second pass: squeeze whitespace adjacent to the punctuation
    // tokens (`/`, `//`, `::`, `@`).  Whitespace adjacent to a
    // *single* `:` (prefix-separator within a QName) is preserved —
    // a QName is a single token in XPath, so `xpns :*` is malformed
    // and must stay malformed for the name-test parser to reject.
    let mut compacted = String::with_capacity(out.len());
    let bytes = out.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if c == ' ' {
            let prev = compacted.chars().last();
            let next = bytes.get(i + 1).map(|b| *b as char);
            let prev2: Option<char> = {
                let s = compacted.as_str();
                let mut chars = s.chars().rev();
                chars.next(); chars.next()
            };
            let next2 = bytes.get(i + 2).map(|b| *b as char);
            // `::` axis specifier — strip whitespace around either ':'.
            let prev_is_axis = matches!((prev2, prev), (Some(':'), Some(':')));
            let next_is_axis = matches!((next, next2), (Some(':'), Some(':')));
            let touches_struct = matches!(prev, Some('/') | Some('@'))
                || matches!(next, Some('/') | Some('@'))
                || prev_is_axis
                || next_is_axis;
            if touches_struct { i += 1; continue; }
        }
        compacted.push(c);
        i += 1;
    }
    compacted
}

// ── tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn no_prefixes(_: &str) -> Option<String> { None }

    #[test]
    fn parses_simple_child_path() {
        let p = parse_selector("child", &no_prefixes).unwrap();
        assert_eq!(p.paths.len(), 1);
        assert_eq!(p.paths[0].steps.len(), 1);
        assert!(matches!(&p.paths[0].steps[0], PathStep::Child(NameTest::Name(_))));
        assert!(!p.paths[0].descendant);
    }

    #[test]
    fn parses_descendant_prefix() {
        let p = parse_selector(".//foo/bar", &no_prefixes).unwrap();
        assert!(p.paths[0].descendant);
        assert_eq!(p.paths[0].steps.len(), 2);
    }

    #[test]
    fn parses_dot_self() {
        let p = parse_selector(".", &no_prefixes).unwrap();
        assert_eq!(p.paths[0].steps.len(), 0);
    }

    #[test]
    fn parses_wildcard() {
        let p = parse_selector("*", &no_prefixes).unwrap();
        assert!(matches!(&p.paths[0].steps[0], PathStep::Child(NameTest::Any)));
    }

    #[test]
    fn parses_field_with_attribute() {
        let f = parse_field("@id", &no_prefixes).unwrap();
        assert_eq!(f.paths[0].steps.len(), 1);
        assert!(matches!(&f.paths[0].steps[0], PathStep::Attribute(_)));
        assert!(f.all_attribute);
    }

    #[test]
    fn parses_field_with_child_then_attribute() {
        let f = parse_field("foo/@id", &no_prefixes).unwrap();
        assert_eq!(f.paths[0].steps.len(), 2);
        assert!(matches!(&f.paths[0].steps[0], PathStep::Child(_)));
        assert!(matches!(&f.paths[0].steps[1], PathStep::Attribute(_)));
        assert!(f.all_attribute);
    }

    #[test]
    fn rejects_attribute_in_selector() {
        assert!(parse_selector("@id", &no_prefixes).is_err());
        assert!(parse_selector("foo/@id", &no_prefixes).is_err());
    }

    #[test]
    fn rejects_attribute_mid_path() {
        assert!(parse_field("@id/foo", &no_prefixes).is_err());
    }

    #[test]
    fn parses_union() {
        let p = parse_selector("foo | bar | baz", &no_prefixes).unwrap();
        assert_eq!(p.paths.len(), 3);
    }

    #[test]
    fn resolves_prefixed_names() {
        let resolver = |p: &str| if p == "ns" { Some("urn:x".into()) } else { None };
        let p = parse_selector("ns:foo", &resolver).unwrap();
        match &p.paths[0].steps[0] {
            PathStep::Child(NameTest::Name(qn)) => {
                assert_eq!(qn.namespace.as_deref(), Some("urn:x"));
                assert_eq!(qn.local.as_ref(), "foo");
            }
            _ => panic!(),
        }
    }

    #[test]
    fn rejects_undeclared_prefix() {
        assert!(parse_selector("unknown:foo", &no_prefixes).is_err());
    }

    #[test]
    fn parses_namespace_wildcard() {
        let resolver = |p: &str| if p == "ns" { Some("urn:x".into()) } else { None };
        let p = parse_selector("ns:*", &resolver).unwrap();
        assert!(matches!(&p.paths[0].steps[0], PathStep::Child(NameTest::AnyInNs(_))));
    }
}
