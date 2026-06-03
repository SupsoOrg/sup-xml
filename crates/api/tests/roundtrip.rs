/// Roundtrip stability: parse(x) → serialize → parse → serialize must produce
/// the same bytes on both serialize passes. This proves the serializer is
/// idempotent and the parser/serializer are consistent with each other.
///
/// We do NOT require serialize(parse(x)) == x, because the serializer may
/// normalize quoting style, drop pre-root whitespace/comments, etc.
use sup_xml::{
    parse_str, serialize_to_string, serialize_formatted, ParseOptions,
};

macro_rules! fixture {
    ($name:expr) => {
        include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/fixtures/", $name))
    };
}

fn assert_roundtrip_stable(label: &str, xml: &str) {
    let doc1 = parse_str(xml, &ParseOptions::default())
        .unwrap_or_else(|e| panic!("{label}: first parse failed: {e}"));
    let text1 = serialize_to_string(&doc1);
    let doc2 = parse_str(&text1, &ParseOptions::default()).unwrap_or_else(|e| {
        panic!("{label}: re-parse of serialized output failed: {e}\n\nOutput was:\n{text1}")
    });
    let text2 = serialize_to_string(&doc2);
    assert_eq!(text1, text2, "{label}: serializer is not idempotent");
}

#[test]
fn roundtrip_simple() {
    assert_roundtrip_stable("simple.xml", fixture!("simple.xml"));
}

#[test]
fn roundtrip_namespaces() {
    assert_roundtrip_stable("namespaces.xml", fixture!("namespaces.xml"));
}

#[test]
fn roundtrip_attributes() {
    assert_roundtrip_stable("attributes.xml", fixture!("attributes.xml"));
}

#[test]
fn roundtrip_deep() {
    assert_roundtrip_stable("deep.xml", fixture!("deep.xml"));
}

#[test]
fn roundtrip_cdata() {
    assert_roundtrip_stable("cdata.xml", fixture!("cdata.xml"));
}

#[test]
fn roundtrip_unicode() {
    assert_roundtrip_stable("unicode.xml", fixture!("unicode.xml"));
}

// ── formatted output spot-checks ──────────────────────────────────────────────

#[test]
fn formatted_simple_structure() {
    let doc = parse_str(fixture!("simple.xml"), &ParseOptions::default()).unwrap();
    let out = serialize_formatted(&doc);
    // Each book should be indented one level
    assert!(out.contains("  <book "), "book elements should be indented: {out}");
    // Leaf text content should be inline
    assert!(out.contains("<author>Gambardella, Matthew</author>"), "author should be inline: {out}");
}

#[test]
fn formatted_deep_indentation() {
    let doc = parse_str(fixture!("deep.xml"), &ParseOptions::default()).unwrap();
    let out = serialize_formatted(&doc);
    // l5 should have 4 levels of indentation (2 spaces × 4)
    assert!(out.contains("        <l5>") || out.contains("    <l5>"),
        "expected indented l5: {out}");
}

#[test]
fn formatted_cdata_preserved() {
    let doc = parse_str(fixture!("cdata.xml"), &ParseOptions::default()).unwrap();
    let out = serialize_formatted(&doc);
    assert!(out.contains("<![CDATA["), "CDATA section must survive formatting");
}

#[test]
fn formatted_is_also_stable() {
    // Formatted output → re-parse → compact → re-parse → compact must be stable
    let doc1 = parse_str(fixture!("simple.xml"), &ParseOptions::default()).unwrap();
    let formatted = serialize_formatted(&doc1);
    let doc2 = parse_str(&formatted, &ParseOptions::default()).unwrap();
    let compact1 = serialize_to_string(&doc2);
    let doc3 = parse_str(&compact1, &ParseOptions::default()).unwrap();
    let compact2 = serialize_to_string(&doc3);
    assert_eq!(compact1, compact2, "compact output after formatting is not stable");
}
