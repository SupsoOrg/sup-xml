//! DTD validation walk.
//!
//! Given a [`Document`] and its captured [`Dtd`], check every element
//! and attribute against the declarations.  Returns the first error
//! encountered (libxml2 stops at first fatal error in
//! `xmlValidateDocument` too).
//!
//! # Algorithm
//!
//! Two passes:
//!
//! 1. **Tree walk** — for each element node:
//!    * Look up its `<!ELEMENT>` decl.  If absent in a non-empty DTD,
//!      that's an error (libxml2's "No declaration for element foo").
//!    * Match its child sequence against the content model.
//!    * Check each attribute against the corresponding `<!ATTLIST>`:
//!      required ones present, enumerated values valid, FIXED values
//!      equal, IDs unique, IDREFs recorded for pass 2.
//! 2. **Cross-ref pass** — every IDREF target collected in pass 1
//!    must equal some ID seen anywhere in pass 1.
//!
//! The content-model matcher is a small recursive descent over the
//! [`Group`] tree.  We don't bother with NFA→DFA conversion: real
//! DTD content models are tiny (rarely more than a few dozen
//! particles), and recursive matching with greedy/lazy fallback on
//! `?`/`*`/`+` handles them in microseconds.

use std::collections::{HashMap, HashSet};

use sup_xml_tree::dom::{Document, Node, NodeKind};

use super::{
    model::{ContentModel, Group, GroupKind, Item, Occurrence, Particle},
    AttDecl, AttDefault, AttType, Dtd,
};

/// One DTD validation failure.  Mirrors libxml2's per-error
/// structure shape just enough for the compat layer to surface.
#[derive(Debug, Clone)]
pub struct DtdError {
    pub element: String,
    pub message: String,
}

impl std::fmt::Display for DtdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "DTD validation error on element <{}>: {}", self.element, self.message)
    }
}

impl std::error::Error for DtdError {}

/// Validate `doc` against `dtd`.  Returns `Ok(())` if every
/// declaration was satisfied; otherwise an `Err` carrying *every*
/// error found in document order.
///
/// libxml2's `xmlValidateDocument` historically returned only a
/// single boolean, but its structured-error callback fires for
/// each violation.  We surface the full list directly so consumers
/// can dump them via lxml's `dtd.error_log` without depending on
/// callback installation.
///
/// An empty DTD (`dtd.is_empty()`) trivially passes.
pub fn validate(doc: &Document, dtd: &Dtd) -> Result<(), Vec<DtdError>> {
    if dtd.is_empty() {
        return Ok(());
    }
    let mut ids:    HashSet<String> = HashSet::new();
    let mut idrefs: Vec<(String, String)> = Vec::new();
    let mut errors: Vec<DtdError> = Vec::new();
    let root = doc.root();
    walk(root, dtd, &mut ids, &mut idrefs, &mut errors);
    for (element, idref) in idrefs {
        if !ids.contains(&idref) {
            errors.push(DtdError {
                element,
                message: format!("IDREF '{}' does not match any declared ID", idref),
            });
        }
    }
    if errors.is_empty() { Ok(()) } else { Err(errors) }
}

/// Convenience for the common "did it validate?" yes/no question.
/// Equivalent to `validate(doc, dtd).is_ok()`.
pub fn is_valid(doc: &Document, dtd: &Dtd) -> bool {
    validate(doc, dtd).is_ok()
}

fn walk(
    node:   &Node<'_>,
    dtd:    &Dtd,
    ids:    &mut HashSet<String>,
    idrefs: &mut Vec<(String, String)>,
    errors: &mut Vec<DtdError>,
) {
    if !node.is_element() { return; }

    let name = node.name();

    // 1. Element decl lookup.
    let Some(decl) = dtd.elements.get(name) else {
        errors.push(DtdError {
            element: name.to_string(),
            message: "no declaration found".into(),
        });
        // Continue walking children — the doc may still have valid
        // sub-trees, and we want to report all errors in one go.
        for child in node.children() {
            walk(child, dtd, ids, idrefs, errors);
        }
        return;
    };

    // 2. Content-model check.
    if let Err(e) = check_content(node, &decl.content) {
        errors.push(e);
    }

    // 3. Attribute check.
    if let Some(attlist) = dtd.attlists.get(name) {
        check_attrs(node, attlist, ids, idrefs, errors);
    }

    // 4. Recurse into children.
    for child in node.children() {
        walk(child, dtd, ids, idrefs, errors);
    }
}

/// Verify `element`'s children satisfy `model`.  PCDATA = any
/// run of Text/CData nodes; non-element non-text/non-cdata
/// children (Comment, PI) are skipped — they're allowed everywhere
/// per § 2.5.
fn check_content(element: &Node<'_>, model: &ContentModel) -> Result<(), DtdError> {
    match model {
        ContentModel::Empty => {
            for child in element.children() {
                match child.kind {
                    NodeKind::Comment | NodeKind::Pi => continue,
                    NodeKind::Text | NodeKind::CData => {
                        // Whitespace-only text is allowed in EMPTY per
                        // libxml2's permissive mode; we follow suit.
                        if !child.content().chars().all(char::is_whitespace) {
                            return Err(DtdError {
                                element: element.name().to_string(),
                                message: "content not allowed in EMPTY element".into(),
                            });
                        }
                    }
                    _ => return Err(DtdError {
                        element: element.name().to_string(),
                        message: "content not allowed in EMPTY element".into(),
                    }),
                }
            }
            Ok(())
        }
        ContentModel::Any => Ok(()),
        ContentModel::Mixed { choices } => {
            for child in element.children() {
                match child.kind {
                    NodeKind::Comment | NodeKind::Pi
                    | NodeKind::Text  | NodeKind::CData => continue,
                    NodeKind::Element => {
                        if !choices.iter().any(|n| n == child.name()) {
                            return Err(DtdError {
                                element: element.name().to_string(),
                                message: format!(
                                    "child <{}> not allowed in mixed content (allowed: {})",
                                    child.name(),
                                    if choices.is_empty() { "#PCDATA only".into() }
                                    else { choices.join(", ") }
                                ),
                            });
                        }
                    }
                    _ => return Err(DtdError {
                        element: element.name().to_string(),
                        message: "unexpected node kind in mixed content".into(),
                    }),
                }
            }
            Ok(())
        }
        ContentModel::Children(group) => {
            // Collect element children only — text in a children
            // model is forbidden, except for whitespace per § 3.2.1
            // (which libxml2 silently strips).
            let mut child_names: Vec<&str> = Vec::new();
            for child in element.children() {
                match child.kind {
                    NodeKind::Element => child_names.push(child.name()),
                    NodeKind::Comment | NodeKind::Pi => continue,
                    NodeKind::Text | NodeKind::CData
                        if !child.content().chars().all(char::is_whitespace) =>
                    {
                        return Err(DtdError {
                            element: element.name().to_string(),
                            message: "character data not allowed in element-only content".into(),
                        });
                    }
                    NodeKind::Text | NodeKind::CData => {}
                    _ => {}
                }
            }
            let mut pos = 0usize;
            if !match_group(group, &child_names, &mut pos) || pos != child_names.len() {
                return Err(DtdError {
                    element: element.name().to_string(),
                    message: format!(
                        "child sequence [{}] does not match content model",
                        child_names.join(", ")
                    ),
                });
            }
            Ok(())
        }
    }
}

/// Recursive-descent matcher for children content models.  Tries
/// the greedy interpretation first; backtracks on failure for
/// optional/star/plus particles.
///
/// Returns `true` if at least one matching of `group` against
/// `names[pos..]` exists; `pos` advances to the end of the
/// successful match.
fn match_group(group: &Group, names: &[&str], pos: &mut usize) -> bool {
    let start = *pos;
    let min = match group.occur {
        Occurrence::One | Occurrence::OneOrMore => 1,
        Occurrence::ZeroOrOne | Occurrence::ZeroOrMore => 0,
    };
    let max = match group.occur {
        Occurrence::One | Occurrence::ZeroOrOne => 1,
        Occurrence::OneOrMore | Occurrence::ZeroOrMore => usize::MAX,
    };
    let mut matches = 0;
    loop {
        let try_pos = *pos;
        if matches >= max || !match_group_one(group, names, pos) {
            *pos = try_pos;
            break;
        }
        matches += 1;
    }
    if matches < min {
        *pos = start;
        return false;
    }
    true
}

/// Match exactly one iteration of `group` (without the outer
/// occurrence indicator).
fn match_group_one(group: &Group, names: &[&str], pos: &mut usize) -> bool {
    match group.kind {
        GroupKind::Sequence => {
            let start = *pos;
            for item in &group.items {
                if !match_particle(item, names, pos) {
                    *pos = start;
                    return false;
                }
            }
            true
        }
        GroupKind::Choice => {
            let start = *pos;
            for item in &group.items {
                let mut local = start;
                if match_particle(item, names, &mut local) {
                    *pos = local;
                    return true;
                }
            }
            *pos = start;
            false
        }
    }
}

fn match_particle(p: &Particle, names: &[&str], pos: &mut usize) -> bool {
    let start = *pos;
    let min = match p.occur {
        Occurrence::One | Occurrence::OneOrMore => 1,
        Occurrence::ZeroOrOne | Occurrence::ZeroOrMore => 0,
    };
    let max = match p.occur {
        Occurrence::One | Occurrence::ZeroOrOne => 1,
        Occurrence::OneOrMore | Occurrence::ZeroOrMore => usize::MAX,
    };
    let mut matches = 0;
    loop {
        let try_pos = *pos;
        let advanced = match &p.item {
            Item::Name(n) => {
                if names.get(*pos) == Some(&n.as_str()) { *pos += 1; true } else { false }
            }
            Item::Group(g) => match_group(g, names, pos),
        };
        if !advanced || matches >= max {
            *pos = try_pos;
            break;
        }
        matches += 1;
    }
    if matches < min { *pos = start; return false; }
    true
}

fn check_attrs(
    element: &Node<'_>,
    attlist: &[AttDecl],
    ids:    &mut HashSet<String>,
    idrefs: &mut Vec<(String, String)>,
    errors: &mut Vec<DtdError>,
) {
    // Index present attributes by name for O(1) lookup.
    let mut present: HashMap<&str, &str> = HashMap::new();
    for a in element.attributes() {
        let name = a.name();
        // libxml2 ignores `xmlns` and `xmlns:*` in attribute
        // validation — they're namespace declarations, not user
        // attributes.
        if name == "xmlns" || name.starts_with("xmlns:") { continue; }
        present.insert(name, a.value());
    }

    for decl in attlist {
        let value = present.get(decl.name.as_str()).copied();

        // Default-decl checks.
        match &decl.default {
            AttDefault::Required => {
                if value.is_none() {
                    errors.push(DtdError {
                        element: element.name().to_string(),
                        message: format!("required attribute '{}' missing", decl.name),
                    });
                    continue;
                }
            }
            AttDefault::Fixed(want) => {
                if let Some(got) = value {
                    if got != want {
                        errors.push(DtdError {
                            element: element.name().to_string(),
                            message: format!(
                                "attribute '{}' = '{}' violates #FIXED \"{}\"",
                                decl.name, got, want
                            ),
                        });
                        continue;
                    }
                }
            }
            AttDefault::Implied | AttDefault::Default(_) => {}
        }

        // Type checks — only meaningful when a value is present.
        if let Some(v) = value {
            if let Err(e) = check_att_type(element.name(), &decl.name, &decl.att_type, v, ids, idrefs) {
                errors.push(e);
            }
        }
    }
}

fn check_att_type(
    element: &str,
    attr:    &str,
    ty:      &AttType,
    value:   &str,
    ids:     &mut HashSet<String>,
    idrefs:  &mut Vec<(String, String)>,
) -> Result<(), DtdError> {
    match ty {
        AttType::CData => Ok(()),
        AttType::Id => {
            if !is_valid_name(value) {
                return Err(DtdError {
                    element: element.into(),
                    message: format!("attribute '{}' = '{}' is not a valid ID Name", attr, value),
                });
            }
            if !ids.insert(value.to_string()) {
                return Err(DtdError {
                    element: element.into(),
                    message: format!("duplicate ID '{}'", value),
                });
            }
            Ok(())
        }
        AttType::IdRef => {
            if !is_valid_name(value) {
                return Err(DtdError {
                    element: element.into(),
                    message: format!("attribute '{}' = '{}' is not a valid IDREF Name", attr, value),
                });
            }
            idrefs.push((element.to_string(), value.to_string()));
            Ok(())
        }
        AttType::IdRefs => {
            for tok in value.split_ascii_whitespace() {
                if !is_valid_name(tok) {
                    return Err(DtdError {
                        element: element.into(),
                        message: format!(
                            "attribute '{}' IDREFS token '{}' is not a valid Name", attr, tok
                        ),
                    });
                }
                idrefs.push((element.to_string(), tok.to_string()));
            }
            Ok(())
        }
        AttType::Nmtoken => {
            if !is_valid_nmtoken(value) {
                return Err(DtdError {
                    element: element.into(),
                    message: format!("attribute '{}' = '{}' is not a valid NMTOKEN", attr, value),
                });
            }
            Ok(())
        }
        AttType::Nmtokens => {
            for tok in value.split_ascii_whitespace() {
                if !is_valid_nmtoken(tok) {
                    return Err(DtdError {
                        element: element.into(),
                        message: format!(
                            "attribute '{}' NMTOKENS token '{}' invalid", attr, tok
                        ),
                    });
                }
            }
            Ok(())
        }
        AttType::Entity | AttType::Entities => {
            // We don't track entity declarations cross-validation here.
            // Accept any Name(s).
            for tok in value.split_ascii_whitespace() {
                if !is_valid_name(tok) {
                    return Err(DtdError {
                        element: element.into(),
                        message: format!(
                            "attribute '{}' ENTITY token '{}' is not a valid Name", attr, tok
                        ),
                    });
                }
            }
            Ok(())
        }
        AttType::Notation(allowed) | AttType::Enumeration(allowed) => {
            if !allowed.iter().any(|v| v == value) {
                return Err(DtdError {
                    element: element.into(),
                    message: format!(
                        "attribute '{}' = '{}' not in enumeration ({})",
                        attr, value, allowed.join(" | ")
                    ),
                });
            }
            Ok(())
        }
    }
}

/// True if `s` is a valid XML Name (§ 2.3 [4][5]).  Simplified —
/// requires Letter/`_`/`:` start then NameChars; doesn't enforce
/// the full Unicode class, but matches every real-world ID/IDREF.
fn is_valid_name(s: &str) -> bool {
    let mut chars = s.chars();
    let first = match chars.next() { Some(c) => c, None => return false };
    if !is_name_start_char(first) { return false; }
    chars.all(is_name_char)
}

fn is_valid_nmtoken(s: &str) -> bool {
    !s.is_empty() && s.chars().all(is_name_char)
}

fn is_name_start_char(c: char) -> bool {
    matches!(c, 'A'..='Z' | 'a'..='z' | '_' | ':')
        || (c as u32) >= 0xC0
}

fn is_name_char(c: char) -> bool {
    is_name_start_char(c) || matches!(c, '0'..='9' | '-' | '.')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::options::ParseOptions;
    use crate::parser::parse_bytes_with_dtd;

    fn parse(src: &str) -> (sup_xml_tree::dom::Document, Dtd) {
        let opts = ParseOptions { namespace_aware: false, ..ParseOptions::default() };
        parse_bytes_with_dtd(src.as_bytes(), &opts).expect("parse failed")
    }

    fn assert_err_contains(errs: &[DtdError], needle: &str) {
        assert!(
            errs.iter().any(|e| e.message.contains(needle)),
            "no error matching {needle:?}, got: {errs:#?}",
        );
    }

    // ── infrastructure: Display, is_valid, empty DTD ─────────────────

    #[test]
    fn dtd_error_display_format() {
        let e = DtdError {
            element: "foo".into(),
            message: "boom".into(),
        };
        assert_eq!(format!("{e}"), "DTD validation error on element <foo>: boom");
    }

    #[test]
    fn empty_dtd_validates_anything() {
        let opts = ParseOptions::default();
        let doc = crate::parse_str("<r><a/><b/></r>", &opts).unwrap();
        let dtd = Dtd::new();
        assert!(dtd.is_empty());
        assert!(validate(&doc, &dtd).is_ok());
        assert!(is_valid(&doc, &dtd));
    }

    #[test]
    fn is_valid_returns_false_on_failure() {
        // Element 'r' is declared but 'unknown' is not.
        let src = r#"<?xml version="1.0"?>
<!DOCTYPE r [
  <!ELEMENT r ANY>
]>
<r><unknown/></r>"#;
        let (doc, dtd) = parse(src);
        assert!(!is_valid(&doc, &dtd));
    }

    // ── walk: missing declaration, recurses into children ────────────

    #[test]
    fn missing_element_decl_reports_error_but_keeps_walking() {
        let src = r#"<?xml version="1.0"?>
<!DOCTYPE r [
  <!ELEMENT r ANY>
  <!ELEMENT b ANY>
]>
<r><undeclared><b/></undeclared></r>"#;
        // 'undeclared' has no decl, but 'b' inside it does — the walker
        // must continue past 'undeclared' so the doc doesn't silently
        // pass for descendants.
        let (doc, dtd) = parse(src);
        let errs = validate(&doc, &dtd).unwrap_err();
        assert_err_contains(&errs, "no declaration found");
    }

    // ── EMPTY content model ──────────────────────────────────────────

    #[test]
    fn empty_model_accepts_whitespace_text_and_comments() {
        let src = r#"<?xml version="1.0"?>
<!DOCTYPE r [
  <!ELEMENT r EMPTY>
]>
<r>   <!-- comment --><?pi?></r>"#;
        let (doc, dtd) = parse(src);
        assert!(validate(&doc, &dtd).is_ok());
    }

    #[test]
    fn empty_model_rejects_non_whitespace_text() {
        let src = r#"<?xml version="1.0"?>
<!DOCTYPE r [
  <!ELEMENT r EMPTY>
]>
<r>text</r>"#;
        let (doc, dtd) = parse(src);
        let errs = validate(&doc, &dtd).unwrap_err();
        assert_err_contains(&errs, "EMPTY");
    }

    #[test]
    fn empty_model_rejects_child_elements() {
        let src = r#"<?xml version="1.0"?>
<!DOCTYPE r [
  <!ELEMENT r EMPTY>
  <!ELEMENT a EMPTY>
]>
<r><a/></r>"#;
        let (doc, dtd) = parse(src);
        let errs = validate(&doc, &dtd).unwrap_err();
        assert_err_contains(&errs, "EMPTY");
    }

    // ── ANY content model ────────────────────────────────────────────

    #[test]
    fn any_model_accepts_everything() {
        let src = r#"<?xml version="1.0"?>
<!DOCTYPE r [
  <!ELEMENT r ANY>
  <!ELEMENT a ANY>
  <!ELEMENT b ANY>
]>
<r>text<a/>more<b><a/></b></r>"#;
        let (doc, dtd) = parse(src);
        assert!(validate(&doc, &dtd).is_ok(), "got {:?}", validate(&doc, &dtd));
    }

    // ── Mixed content ────────────────────────────────────────────────

    #[test]
    fn mixed_accepts_listed_children() {
        let src = r#"<?xml version="1.0"?>
<!DOCTYPE r [
  <!ELEMENT r (#PCDATA | em | b)*>
  <!ELEMENT em (#PCDATA)>
  <!ELEMENT b (#PCDATA)>
]>
<r>text <em>emp</em> tail <b>bold</b></r>"#;
        let (doc, dtd) = parse(src);
        assert!(validate(&doc, &dtd).is_ok());
    }

    #[test]
    fn mixed_rejects_unlisted_child() {
        let src = r#"<?xml version="1.0"?>
<!DOCTYPE r [
  <!ELEMENT r (#PCDATA | em)*>
  <!ELEMENT em (#PCDATA)>
  <!ELEMENT bad (#PCDATA)>
]>
<r>text <bad/></r>"#;
        let (doc, dtd) = parse(src);
        let errs = validate(&doc, &dtd).unwrap_err();
        assert_err_contains(&errs, "mixed content");
    }

    #[test]
    fn mixed_pcdata_only_lists_pcdata_only_in_error() {
        let src = r#"<?xml version="1.0"?>
<!DOCTYPE r [
  <!ELEMENT r (#PCDATA)>
  <!ELEMENT bad (#PCDATA)>
]>
<r>text <bad/></r>"#;
        let (doc, dtd) = parse(src);
        let errs = validate(&doc, &dtd).unwrap_err();
        assert_err_contains(&errs, "#PCDATA only");
    }

    // ── Children content (sequences, choices, occurrences) ──────────

    #[test]
    fn children_sequence_matches_in_order() {
        let src = r#"<?xml version="1.0"?>
<!DOCTYPE r [
  <!ELEMENT r (a, b)>
  <!ELEMENT a EMPTY>
  <!ELEMENT b EMPTY>
]>
<r><a/><b/></r>"#;
        let (doc, dtd) = parse(src);
        assert!(validate(&doc, &dtd).is_ok());
    }

    #[test]
    fn children_sequence_wrong_order_fails() {
        let src = r#"<?xml version="1.0"?>
<!DOCTYPE r [
  <!ELEMENT r (a, b)>
  <!ELEMENT a EMPTY>
  <!ELEMENT b EMPTY>
]>
<r><b/><a/></r>"#;
        let (doc, dtd) = parse(src);
        let errs = validate(&doc, &dtd).unwrap_err();
        assert_err_contains(&errs, "does not match content model");
    }

    #[test]
    fn children_choice_accepts_either() {
        let src = r#"<?xml version="1.0"?>
<!DOCTYPE r [
  <!ELEMENT r (a | b)>
  <!ELEMENT a EMPTY>
  <!ELEMENT b EMPTY>
]>
<r><b/></r>"#;
        let (doc, dtd) = parse(src);
        assert!(validate(&doc, &dtd).is_ok());
    }

    #[test]
    fn children_choice_rejects_neither() {
        let src = r#"<?xml version="1.0"?>
<!DOCTYPE r [
  <!ELEMENT r (a | b)>
  <!ELEMENT a EMPTY>
  <!ELEMENT b EMPTY>
  <!ELEMENT c EMPTY>
]>
<r><c/></r>"#;
        let (doc, dtd) = parse(src);
        assert!(validate(&doc, &dtd).is_err());
    }

    #[test]
    fn children_optional_allows_zero_or_one() {
        let src = r#"<?xml version="1.0"?>
<!DOCTYPE r [
  <!ELEMENT r (a?)>
  <!ELEMENT a EMPTY>
]>
<r></r>"#;
        let (doc, dtd) = parse(src);
        assert!(validate(&doc, &dtd).is_ok());

        let src2 = r#"<?xml version="1.0"?>
<!DOCTYPE r [
  <!ELEMENT r (a?)>
  <!ELEMENT a EMPTY>
]>
<r><a/></r>"#;
        let (doc2, dtd2) = parse(src2);
        assert!(validate(&doc2, &dtd2).is_ok());
    }

    #[test]
    fn children_zero_or_more_allows_repetition() {
        let src = r#"<?xml version="1.0"?>
<!DOCTYPE r [
  <!ELEMENT r (a*)>
  <!ELEMENT a EMPTY>
]>
<r><a/><a/><a/></r>"#;
        let (doc, dtd) = parse(src);
        assert!(validate(&doc, &dtd).is_ok());
    }

    #[test]
    fn children_one_or_more_requires_at_least_one() {
        let src = r#"<?xml version="1.0"?>
<!DOCTYPE r [
  <!ELEMENT r (a+)>
  <!ELEMENT a EMPTY>
]>
<r></r>"#;
        let (doc, dtd) = parse(src);
        assert!(validate(&doc, &dtd).is_err());

        let src2 = r#"<?xml version="1.0"?>
<!DOCTYPE r [
  <!ELEMENT r (a+)>
  <!ELEMENT a EMPTY>
]>
<r><a/><a/></r>"#;
        let (doc2, dtd2) = parse(src2);
        assert!(validate(&doc2, &dtd2).is_ok());
    }

    #[test]
    fn children_nested_group() {
        let src = r#"<?xml version="1.0"?>
<!DOCTYPE r [
  <!ELEMENT r ((a, b)+ , c?)>
  <!ELEMENT a EMPTY>
  <!ELEMENT b EMPTY>
  <!ELEMENT c EMPTY>
]>
<r><a/><b/><a/><b/><c/></r>"#;
        let (doc, dtd) = parse(src);
        assert!(validate(&doc, &dtd).is_ok());
    }

    #[test]
    fn children_text_rejected_unless_whitespace() {
        let src = r#"<?xml version="1.0"?>
<!DOCTYPE r [
  <!ELEMENT r (a)>
  <!ELEMENT a EMPTY>
]>
<r>nope<a/></r>"#;
        let (doc, dtd) = parse(src);
        let errs = validate(&doc, &dtd).unwrap_err();
        assert_err_contains(&errs, "character data");
    }

    #[test]
    fn children_whitespace_text_is_ignored() {
        let src = r#"<?xml version="1.0"?>
<!DOCTYPE r [
  <!ELEMENT r (a)>
  <!ELEMENT a EMPTY>
]>
<r>
  <a/>
</r>"#;
        let (doc, dtd) = parse(src);
        assert!(validate(&doc, &dtd).is_ok());
    }

    // ── Attribute validation ─────────────────────────────────────────

    #[test]
    fn required_attribute_missing_errors() {
        let src = r#"<?xml version="1.0"?>
<!DOCTYPE r [
  <!ELEMENT r EMPTY>
  <!ATTLIST r id CDATA #REQUIRED>
]>
<r/>"#;
        let (doc, dtd) = parse(src);
        let errs = validate(&doc, &dtd).unwrap_err();
        assert_err_contains(&errs, "required attribute 'id'");
    }

    #[test]
    fn required_attribute_present_ok() {
        let src = r#"<?xml version="1.0"?>
<!DOCTYPE r [
  <!ELEMENT r EMPTY>
  <!ATTLIST r id CDATA #REQUIRED>
]>
<r id="x"/>"#;
        let (doc, dtd) = parse(src);
        assert!(validate(&doc, &dtd).is_ok());
    }

    #[test]
    fn fixed_attribute_mismatch_errors() {
        let src = r#"<?xml version="1.0"?>
<!DOCTYPE r [
  <!ELEMENT r EMPTY>
  <!ATTLIST r kind CDATA #FIXED "v1">
]>
<r kind="v2"/>"#;
        let (doc, dtd) = parse(src);
        let errs = validate(&doc, &dtd).unwrap_err();
        assert_err_contains(&errs, "#FIXED");
    }

    #[test]
    fn fixed_attribute_match_ok() {
        let src = r#"<?xml version="1.0"?>
<!DOCTYPE r [
  <!ELEMENT r EMPTY>
  <!ATTLIST r kind CDATA #FIXED "v1">
]>
<r kind="v1"/>"#;
        let (doc, dtd) = parse(src);
        assert!(validate(&doc, &dtd).is_ok());
    }

    #[test]
    fn enumeration_accepts_listed_value() {
        let src = r#"<?xml version="1.0"?>
<!DOCTYPE r [
  <!ELEMENT r EMPTY>
  <!ATTLIST r role (admin|user|guest) "user">
]>
<r role="admin"/>"#;
        let (doc, dtd) = parse(src);
        assert!(validate(&doc, &dtd).is_ok());
    }

    #[test]
    fn enumeration_rejects_unlisted_value() {
        let src = r#"<?xml version="1.0"?>
<!DOCTYPE r [
  <!ELEMENT r EMPTY>
  <!ATTLIST r role (admin|user|guest) "user">
]>
<r role="root"/>"#;
        let (doc, dtd) = parse(src);
        let errs = validate(&doc, &dtd).unwrap_err();
        assert_err_contains(&errs, "not in enumeration");
    }

    #[test]
    fn id_must_be_valid_name() {
        let src = r#"<?xml version="1.0"?>
<!DOCTYPE r [
  <!ELEMENT r EMPTY>
  <!ATTLIST r id ID #IMPLIED>
]>
<r id="0bad"/>"#;
        let (doc, dtd) = parse(src);
        let errs = validate(&doc, &dtd).unwrap_err();
        assert_err_contains(&errs, "valid ID Name");
    }

    #[test]
    fn duplicate_id_errors() {
        let src = r#"<?xml version="1.0"?>
<!DOCTYPE r [
  <!ELEMENT r (x, y)>
  <!ELEMENT x EMPTY>
  <!ELEMENT y EMPTY>
  <!ATTLIST x id ID #REQUIRED>
  <!ATTLIST y id ID #REQUIRED>
]>
<r><x id="a"/><y id="a"/></r>"#;
        let (doc, dtd) = parse(src);
        let errs = validate(&doc, &dtd).unwrap_err();
        assert_err_contains(&errs, "duplicate ID");
    }

    #[test]
    fn idref_must_match_declared_id() {
        let src = r#"<?xml version="1.0"?>
<!DOCTYPE r [
  <!ELEMENT r (x, y)>
  <!ELEMENT x EMPTY>
  <!ELEMENT y EMPTY>
  <!ATTLIST x id ID #REQUIRED>
  <!ATTLIST y target IDREF #REQUIRED>
]>
<r><x id="a"/><y target="ghost"/></r>"#;
        let (doc, dtd) = parse(src);
        let errs = validate(&doc, &dtd).unwrap_err();
        assert_err_contains(&errs, "IDREF 'ghost'");
    }

    #[test]
    fn idref_resolves_to_existing_id() {
        let src = r#"<?xml version="1.0"?>
<!DOCTYPE r [
  <!ELEMENT r (x, y)>
  <!ELEMENT x EMPTY>
  <!ELEMENT y EMPTY>
  <!ATTLIST x id ID #REQUIRED>
  <!ATTLIST y target IDREF #REQUIRED>
]>
<r><x id="a"/><y target="a"/></r>"#;
        let (doc, dtd) = parse(src);
        assert!(validate(&doc, &dtd).is_ok());
    }

    #[test]
    fn idrefs_validates_each_token() {
        let src = r#"<?xml version="1.0"?>
<!DOCTYPE r [
  <!ELEMENT r (x, x, y)>
  <!ELEMENT x EMPTY>
  <!ELEMENT y EMPTY>
  <!ATTLIST x id ID #REQUIRED>
  <!ATTLIST y refs IDREFS #REQUIRED>
]>
<r><x id="a"/><x id="b"/><y refs="a b"/></r>"#;
        let (doc, dtd) = parse(src);
        assert!(validate(&doc, &dtd).is_ok());
    }

    #[test]
    fn idrefs_rejects_invalid_token() {
        let src = r#"<?xml version="1.0"?>
<!DOCTYPE r [
  <!ELEMENT r (x, y)>
  <!ELEMENT x EMPTY>
  <!ELEMENT y EMPTY>
  <!ATTLIST x id ID #REQUIRED>
  <!ATTLIST y refs IDREFS #REQUIRED>
]>
<r><x id="a"/><y refs="a 0bad"/></r>"#;
        let (doc, dtd) = parse(src);
        let errs = validate(&doc, &dtd).unwrap_err();
        assert_err_contains(&errs, "valid Name");
    }

    #[test]
    fn nmtoken_validation() {
        let src = r#"<?xml version="1.0"?>
<!DOCTYPE r [
  <!ELEMENT r EMPTY>
  <!ATTLIST r tok NMTOKEN #IMPLIED>
]>
<r tok="bad token"/>"#;
        // Space makes it not a valid single NMTOKEN.
        let (doc, dtd) = parse(src);
        let errs = validate(&doc, &dtd).unwrap_err();
        assert_err_contains(&errs, "NMTOKEN");
    }

    #[test]
    fn nmtoken_accepts_digit_start() {
        let src = r#"<?xml version="1.0"?>
<!DOCTYPE r [
  <!ELEMENT r EMPTY>
  <!ATTLIST r tok NMTOKEN #IMPLIED>
]>
<r tok="123abc"/>"#;
        let (doc, dtd) = parse(src);
        assert!(validate(&doc, &dtd).is_ok());
    }

    #[test]
    fn nmtokens_validates_each_token() {
        let src = r#"<?xml version="1.0"?>
<!DOCTYPE r [
  <!ELEMENT r EMPTY>
  <!ATTLIST r toks NMTOKENS #IMPLIED>
]>
<r toks="a 1b c-d"/>"#;
        let (doc, dtd) = parse(src);
        assert!(validate(&doc, &dtd).is_ok());
    }

    #[test]
    fn nmtokens_rejects_invalid_token() {
        // The whole attribute string contains an empty token if we have
        // a leading/internal space — but split_ascii_whitespace handles
        // that.  Use an explicitly invalid char like '!'.
        let src = r#"<?xml version="1.0"?>
<!DOCTYPE r [
  <!ELEMENT r EMPTY>
  <!ATTLIST r toks NMTOKENS #IMPLIED>
]>
<r toks="a b!c d"/>"#;
        let (doc, dtd) = parse(src);
        let errs = validate(&doc, &dtd).unwrap_err();
        assert_err_contains(&errs, "NMTOKENS");
    }

    #[test]
    fn entity_attr_accepts_name() {
        let src = r#"<?xml version="1.0"?>
<!DOCTYPE r [
  <!ELEMENT r EMPTY>
  <!ATTLIST r ent ENTITY #IMPLIED>
]>
<r ent="picture"/>"#;
        let (doc, dtd) = parse(src);
        assert!(validate(&doc, &dtd).is_ok());
    }

    #[test]
    fn entity_attr_rejects_invalid_name() {
        let src = r#"<?xml version="1.0"?>
<!DOCTYPE r [
  <!ELEMENT r EMPTY>
  <!ATTLIST r ent ENTITY #IMPLIED>
]>
<r ent="0bad"/>"#;
        let (doc, dtd) = parse(src);
        let errs = validate(&doc, &dtd).unwrap_err();
        assert_err_contains(&errs, "ENTITY token");
    }

    #[test]
    fn xmlns_attributes_are_skipped_during_attribute_validation() {
        // xmlns:foo isn't declared in the ATTLIST but it must not
        // trigger an "unexpected attribute"-style failure.
        let src = r#"<?xml version="1.0"?>
<!DOCTYPE r [
  <!ELEMENT r EMPTY>
  <!ATTLIST r id CDATA #IMPLIED>
]>
<r xmlns:foo="urn:bar" id="x"/>"#;
        let (doc, dtd) = parse(src);
        assert!(validate(&doc, &dtd).is_ok());
    }

    // ── is_valid_name / is_valid_nmtoken (direct) ────────────────────

    #[test]
    fn name_predicates_basic() {
        assert!(is_valid_name("foo"));
        assert!(is_valid_name("_bar"));
        assert!(is_valid_name(":ns"));
        assert!(is_valid_name("a1-2.3"));
        assert!(!is_valid_name(""));
        assert!(!is_valid_name("0foo"));
        assert!(!is_valid_name("a b"));
    }

    #[test]
    fn nmtoken_predicate() {
        assert!(is_valid_nmtoken("1abc"));
        assert!(is_valid_nmtoken("foo-bar"));
        assert!(!is_valid_nmtoken(""));
        assert!(!is_valid_nmtoken("a b"));
        assert!(!is_valid_nmtoken("!"));
    }
}
