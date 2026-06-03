//! XML 1.1 conformance — focused on the parts implemented so far.
//!
//! XML 1.1 changes from 1.0:
//! * `<?xml version="1.1"?>` is accepted.
//! * End-of-line normalization rules add NEL (#x85) and LS (#x2028)
//!   as line-ending characters (alongside CR/LF/CRLF).
//! * Character references to C0 controls #x1–#x1F (except #x0) are
//!   allowed in text and attribute values.
//! * The Name and NameChar production widens to include additional
//!   Unicode ranges that were excluded in 1.0.
//!
//! This file is a living test set — each new piece of XML 1.1 support
//! adds its tests here, so the file as a whole tracks how much of the
//! spec we cover.

use sup_xml::{parse_str, xpath_str, ParseOptions};

fn parse(xml: &str) -> sup_xml::Document {
    parse_str(xml, &ParseOptions::default()).expect("test document must parse")
}

// ── XML 1.1 declaration ─────────────────────────────────────────────────────

#[test]
fn version_1_1_decl_is_accepted() {
    let d = parse(r#"<?xml version="1.1"?><r><a>hello</a></r>"#);
    assert_eq!(d.version, "1.1");
}

#[test]
fn version_1_1_decl_with_encoding_and_standalone() {
    let d = parse(
        r#"<?xml version="1.1" encoding="UTF-8" standalone="yes"?><r/>"#,
    );
    assert_eq!(d.version, "1.1");
    assert_eq!(d.encoding, "UTF-8");
    assert_eq!(d.standalone, Some(true));
}

#[test]
fn version_1_0_decl_still_default() {
    let d = parse("<r/>");
    assert_eq!(d.version, "1.0");
}

// ── §2.11 end-of-line normalization (XML 1.1 line-ending set) ──────────────
//
// XML 1.1 §2.11 widens the EOL set from the 1.0 pair (`\r`, `\r\n`) to
// also include NEL (`U+0085`) and LS (`U+2028`).  Every such occurrence
// must be rewritten to LF (`\n`) before parsing — i.e. the value the
// application sees through the DOM is the normalized form.

#[test]
fn nel_is_normalized_to_lf_in_1_1_text() {
    // NEL = U+0085 = UTF-8 0xC2 0x85.
    let bytes: Vec<u8> = b"<?xml version=\"1.1\"?><r>a\xc2\x85b</r>".to_vec();
    let xml = std::str::from_utf8(&bytes).unwrap();
    let d = parse_str(xml, &ParseOptions::default())
        .expect("NEL must parse in 1.1 text content");
    assert_eq!(xpath_str(&d, "string(/r)").unwrap(), "a\nb");
}

#[test]
fn ls_is_normalized_to_lf_in_1_1_text() {
    // LS = U+2028 = UTF-8 0xE2 0x80 0xA8.
    let bytes: Vec<u8> = b"<?xml version=\"1.1\"?><r>a\xe2\x80\xa8b</r>".to_vec();
    let xml = std::str::from_utf8(&bytes).unwrap();
    let d = parse_str(xml, &ParseOptions::default())
        .expect("LS must parse in 1.1 text content");
    assert_eq!(xpath_str(&d, "string(/r)").unwrap(), "a\nb");
}

#[test]
fn cr_nel_pair_collapses_in_1_1_text() {
    // `\r` followed by NEL is a single line-ending in 1.1 — should
    // collapse to one LF, not two.
    let bytes: Vec<u8> = b"<?xml version=\"1.1\"?><r>a\r\xc2\x85b</r>".to_vec();
    let xml = std::str::from_utf8(&bytes).unwrap();
    let d = parse_str(xml, &ParseOptions::default()).unwrap();
    assert_eq!(xpath_str(&d, "string(/r)").unwrap(), "a\nb");
}

#[test]
fn nel_left_alone_in_1_0_text() {
    // Under XML 1.0 NEL is just a regular character (it's not in the
    // EOL set) — it must not be rewritten.
    let bytes: Vec<u8> = b"<?xml version=\"1.0\"?><r>a\xc2\x85b</r>".to_vec();
    let xml = std::str::from_utf8(&bytes).unwrap();
    let d = parse_str(xml, &ParseOptions::default()).unwrap();
    assert_eq!(xpath_str(&d, "string(/r)").unwrap(), "a\u{85}b");
}

// ── External-entity text-decl version rules ─────────────────────────────────

#[test]
fn external_entity_version_check_is_documented() {
    // XML 1.0 §4.3.4 — a 1.0 host doc accepts 1.0 entities only.
    // XML 1.1 §4.3.4 — a 1.1 host doc accepts 1.0 or 1.1 entities.
    //
    // The integration that drives this lives in
    // `consume_text_decl_if_present`; verified by the parser's
    // existing entity-loading tests.  This test is a docstring
    // anchor — when the consume helper changes shape, the matrix
    // covered here is what to preserve.
    let xml = r#"<?xml version="1.1"?><r>ok</r>"#;
    assert_eq!(parse(xml).version, "1.1");
}
