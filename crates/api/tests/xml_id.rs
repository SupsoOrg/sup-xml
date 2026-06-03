//! W3C `xml:id` Recommendation conformance — when an attribute is
//! named `xml:id` the processor must treat it as an ID, normalize
//! whitespace per the non-CDATA rules, and make the value findable
//! via XPath's `id()` function regardless of any DTD typing.

use sup_xml::{parse_str, xpath_count, xpath_str, ParseOptions};

fn doc(xml: &str) -> sup_xml::Document {
    // Namespace-aware so XPath `@xml:id` resolves through the
    // predefined `xml` prefix.
    let opts = ParseOptions { namespace_aware: true, ..ParseOptions::default() };
    parse_str(xml, &opts).expect("test document must parse")
}

#[test]
fn xml_id_is_lookable_without_dtd() {
    let d = doc(r#"<r><a xml:id="alpha"/><a xml:id="beta"/></r>"#);
    assert_eq!(xpath_count(&d, "id('alpha')").unwrap(), 1);
    assert_eq!(xpath_count(&d, "id('beta')").unwrap(), 1);
    assert_eq!(xpath_count(&d, "id('gamma')").unwrap(), 0);
}

#[test]
fn xml_id_value_is_whitespace_normalized() {
    // W3C xml:id §4 — the attribute is non-CDATA: leading / trailing
    // whitespace is stripped and internal runs collapse to a single
    // space, so `id('foo')` matches `xml:id="  foo  "`.
    let d = doc(r#"<r><a xml:id="  foo  "/></r>"#);
    assert_eq!(xpath_count(&d, "id('foo')").unwrap(), 1);
    // The stored attribute value reflects the normalization.
    assert_eq!(xpath_str(&d, "string(/r/a/@xml:id)").unwrap(), "foo");
}

#[test]
fn xml_id_normalizes_internal_whitespace_runs() {
    let d = doc("<r><a xml:id=\"foo\t\tbar\"/></r>");
    // Internal run of tabs collapses to one space — same convention
    // the DTD non-CDATA pass uses.
    assert_eq!(xpath_str(&d, "string(/r/a/@xml:id)").unwrap(), "foo bar");
}

#[test]
fn xml_id_coexists_with_dtd_attlist_typing() {
    // A document with both DTD-declared ID attributes AND xml:id —
    // both kinds should be lookable via id().
    let d = doc(r#"<!DOCTYPE r [
        <!ELEMENT r (a*)>
        <!ELEMENT a EMPTY>
        <!ATTLIST a key ID #IMPLIED>
    ]>
    <r>
      <a key="legacy"/>
      <a xml:id="modern"/>
    </r>"#);
    assert_eq!(xpath_count(&d, "id('legacy')").unwrap(), 1);
    assert_eq!(xpath_count(&d, "id('modern')").unwrap(), 1);
}

#[test]
fn xml_id_with_other_attributes_isnt_confused() {
    // The `xml:id` rule fires on the literal qualified name only.
    // An unrelated attribute named `id` (no prefix) keeps the
    // libxml2-compatible default-ID convention — used by tools that
    // pre-date xml:id — but is independent of the xml:id mechanism.
    let d = doc(r#"<r><a xml:id="alpha" desc="foo bar"/></r>"#);
    assert_eq!(xpath_str(&d, "string(/r/a/@desc)").unwrap(), "foo bar");
    assert_eq!(xpath_str(&d, "string(/r/a/@xml:id)").unwrap(), "alpha");
    assert_eq!(xpath_count(&d, "id('alpha')").unwrap(), 1);
}
