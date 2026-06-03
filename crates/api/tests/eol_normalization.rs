//! XML 1.0 §2.11 end-of-line normalization.
//!
//! Both `\r\n` and a lone `\r` must be rewritten to `\n` before the
//! application sees the document — this is observable through any
//! API that returns text content (DOM, XPath `string(...)`, etc.).
//!
//! XML 1.1 widens the EOL set further (NEL, LS); those cases live
//! in `xml_1_1.rs`.

use sup_xml::{parse_str, xpath_str, ParseOptions};

fn text_of(xml: &str) -> String {
    let d = parse_str(xml, &ParseOptions::default())
        .expect("test document must parse");
    xpath_str(&d, "string(/r)").unwrap()
}

#[test]
fn crlf_in_text_normalizes_to_lf() {
    assert_eq!(text_of("<r>a\r\nb</r>"), "a\nb");
}

#[test]
fn lone_cr_in_text_normalizes_to_lf() {
    assert_eq!(text_of("<r>a\rb</r>"), "a\nb");
}

#[test]
fn multiple_crlf_runs_normalize() {
    assert_eq!(text_of("<r>a\r\nb\r\nc</r>"), "a\nb\nc");
}

#[test]
fn cr_at_end_of_text_normalizes() {
    assert_eq!(text_of("<r>abc\r</r>"), "abc\n");
}

#[test]
fn lf_alone_is_left_intact() {
    assert_eq!(text_of("<r>a\nb</r>"), "a\nb");
}

#[test]
fn cr_inside_cdata_normalizes_to_lf() {
    // CDATA content is subject to §2.11 normalization too — the spec
    // frames it as happening before parsing recognizes the CDATA
    // delimiters.
    let d = parse_str("<r><![CDATA[a\r\nb]]></r>", &ParseOptions::default()).unwrap();
    assert_eq!(xpath_str(&d, "string(/r)").unwrap(), "a\nb");
}

#[test]
fn cr_char_ref_is_not_normalized() {
    // §4.6: character references survive line-ending normalization —
    // they're the way authors smuggle a literal `\r` into text.
    let d = parse_str("<r>a&#xD;b</r>", &ParseOptions::default()).unwrap();
    assert_eq!(xpath_str(&d, "string(/r)").unwrap(), "a\rb");
}

#[test]
fn cr_lf_char_refs_are_not_normalized() {
    // `&#xD;&#xA;` must remain a two-character sequence, not collapse
    // to a single LF the way raw `\r\n` would.
    let d = parse_str("<r>a&#xD;&#xA;b</r>", &ParseOptions::default()).unwrap();
    assert_eq!(xpath_str(&d, "string(/r)").unwrap(), "a\r\nb");
}

#[test]
fn crlf_split_across_entity_boundary_does_not_merge() {
    // §4.5 says entity replacement text is normalized at entity-
    // definition time, so `&foo;` cannot deliver a `\r` that fuses
    // with a following `\n` in the document text.  The two LFs must
    // remain two LFs after expansion.
    let d = parse_str(
        r#"<!DOCTYPE r [<!ENTITY foo "&#xA;">]><r>a&foo;b</r>"#,
        &ParseOptions::default(),
    ).unwrap();
    let s = xpath_str(&d, "string(/r)").unwrap();
    assert_eq!(s, "a\nb");
}

#[test]
fn cr_followed_by_entity_then_lf_yields_two_lfs() {
    // Source: `a\r&foo;\nb` where foo is empty.  §2.11 says EOL
    // normalization conceptually runs before entity recognition, so
    // the lone `\r` (no `\n` immediately following) becomes one LF,
    // and the bare `\n` after the entity reference stays as its own
    // LF — final string is "a\n\nb".
    let d = parse_str(
        "<!DOCTYPE r [<!ENTITY foo \"\">]><r>a\r&foo;\nb</r>",
        &ParseOptions::default(),
    ).unwrap();
    assert_eq!(xpath_str(&d, "string(/r)").unwrap(), "a\n\nb");
}
