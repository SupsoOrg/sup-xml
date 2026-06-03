/// Edge cases, weird Unicode, and unusual-but-valid XML.
use sup_xml::{parse_str, serialize_to_string, NodeKind, ParseOptions};

macro_rules! fixture {
    ($name:expr) => {
        include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/fixtures/", $name))
    };
}

// ── unusual.xml ───────────────────────────────────────────────────────────────

#[test]
fn unusual_fixture_parses() {
    parse_str(fixture!("unusual.xml"), &ParseOptions::default())
        .expect("unusual.xml should parse cleanly");
}

#[test]
fn unusual_fixture_roundtrip_stable() {
    let doc1 = parse_str(fixture!("unusual.xml"), &ParseOptions::default()).unwrap();
    let text1 = serialize_to_string(&doc1);
    let doc2 = parse_str(&text1, &ParseOptions::default()).unwrap();
    let text2 = serialize_to_string(&doc2);
    assert_eq!(text1, text2, "unusual.xml roundtrip is not stable");
}

// ── BOM handling ──────────────────────────────────────────────────────────────

#[test]
fn utf8_bom_stripped() {
    // U+FEFF (BOM) as the very first byte sequence EF BB BF
    let with_bom = "\u{FEFF}<?xml version=\"1.0\"?><root/>";
    let doc = parse_str(with_bom, &ParseOptions::default())
        .expect("UTF-8 BOM should be silently stripped");
    let root = doc.root();
    assert_eq!(root.kind, NodeKind::Element);
    assert_eq!(root.name(), "root");
}

#[test]
fn bom_only_before_xml_decl() {
    let with_bom = "\u{FEFF}<root/>";
    parse_str(with_bom, &ParseOptions::default())
        .expect("BOM without XML decl should still parse");
}

// ── forbidden character references ────────────────────────────────────────────

#[test]
fn null_char_ref_rejected() {
    // U+0000 is never a valid XML character
    assert!(parse_str("<r>&#x0;</r>", &ParseOptions::default()).is_err(),
        "U+0000 must be rejected");
    assert!(parse_str("<r>&#0;</r>", &ParseOptions::default()).is_err(),
        "U+0000 decimal must be rejected");
}

#[test]
fn control_chars_rejected() {
    for cp in [0x1u32, 0x2, 0x3, 0x7, 0x8, 0xB, 0xC, 0xE, 0xF, 0x1F] {
        let xml = format!("<r>&#x{cp:X};</r>");
        assert!(
            parse_str(&xml, &ParseOptions::default()).is_err(),
            "U+{cp:04X} should be rejected as invalid XML character: {xml}"
        );
    }
}

#[test]
fn tab_lf_cr_are_valid_xml_chars() {
    // These three are the only C0 control chars legal in XML
    parse_str("<r>&#x9;&#xA;&#xD;</r>", &ParseOptions::default())
        .expect("TAB/LF/CR must be accepted");
}

#[test]
fn surrogate_char_refs_rejected() {
    for cp in [0xD800u32, 0xDBFF, 0xDC00, 0xDFFF] {
        let xml = format!("<r>&#x{cp:X};</r>");
        assert!(
            parse_str(&xml, &ParseOptions::default()).is_err(),
            "surrogate U+{cp:04X} must be rejected: {xml}"
        );
    }
}

#[test]
fn fffe_ffff_rejected() {
    // Non-characters excluded by the XML Char production
    assert!(parse_str("<r>&#xFFFE;</r>", &ParseOptions::default()).is_err(),
        "U+FFFE must be rejected");
    assert!(parse_str("<r>&#xFFFF;</r>", &ParseOptions::default()).is_err(),
        "U+FFFF must be rejected");
}

#[test]
fn out_of_unicode_range_rejected() {
    // No Unicode scalar above U+10FFFF exists
    assert!(parse_str("<r>&#x110000;</r>", &ParseOptions::default()).is_err(),
        "U+110000 must be rejected");
    assert!(parse_str("<r>&#x1FFFFF;</r>", &ParseOptions::default()).is_err(),
        "over-range value must be rejected");
}

// ── highest valid codepoints ──────────────────────────────────────────────────

#[test]
fn highest_valid_codepoints_accepted() {
    // U+10FFFF is in the XML 1.0 Char production
    let doc = parse_str("<r>&#x10FFFF;</r>", &ParseOptions::default())
        .expect("U+10FFFF is a valid XML character");
    let root = doc.root();
    assert_eq!(root.kind, NodeKind::Element);
    let text = root.children().next().unwrap().text_content().unwrap();
    assert_eq!(text, "\u{10FFFF}");
}

#[test]
fn supplementary_plane_roundtrip() {
    // A mix of BMP and supplementary codepoints
    let xml = "<r>&#x1D400;&#x20000;&#xE000;&#x10FFFF;</r>";
    let doc1 = parse_str(xml, &ParseOptions::default()).unwrap();
    let out = serialize_to_string(&doc1);
    let doc2 = parse_str(&out, &ParseOptions::default()).unwrap();
    let t1 = doc1.root().children().next().unwrap().text_content().unwrap().to_string();
    let t2 = doc2.root().children().next().unwrap().text_content().unwrap().to_string();
    assert_eq!(t1, t2);
}

// ── duplicate attribute detection ─────────────────────────────────────────────

#[test]
fn duplicate_attr_rejected() {
    assert!(parse_str(r#"<r a="1" a="2"/>"#, &ParseOptions::default()).is_err(),
        "duplicate attribute must be rejected");
}

#[test]
fn duplicate_attr_different_case_accepted() {
    // XML attribute names are case-sensitive: 'A' and 'a' are different
    parse_str(r#"<r A="1" a="2"/>"#, &ParseOptions::default())
        .expect("different-case attribute names are distinct");
}

// ── unusual but valid markup ──────────────────────────────────────────────────

#[test]
fn lt_in_comment_valid() {
    // '<' inside a comment is perfectly legal XML
    parse_str("<r><!-- a < b && b > c --></r>", &ParseOptions::default())
        .expect("< inside comment is valid");
}

#[test]
fn empty_cdata_section() {
    let doc = parse_str("<r><![CDATA[]]></r>", &ParseOptions::default()).unwrap();
    let root = doc.root();
    let child = root.children().next().unwrap();
    assert_eq!(child.kind, NodeKind::CData);
    assert!(child.content().is_empty());
}

#[test]
fn cdata_with_double_bracket_not_ending() {
    // "]]" inside CDATA is fine; only "]]>" ends the section
    let doc = parse_str("<r><![CDATA[a]]b]]>after</r>", &ParseOptions::default()).unwrap();
    let root = doc.root();
    let mut iter = root.children();
    let first = iter.next().unwrap();
    assert_eq!(first.kind, NodeKind::CData);
    assert_eq!(first.content(), "a]]b");
    let second = iter.next().unwrap();
    assert_eq!(second.text_content(), Some("after"));
}

#[test]
fn pi_with_no_content() {
    let doc = parse_str("<?xml version=\"1.0\"?><r><?solo-pi?></r>", &ParseOptions::default()).unwrap();
    let root = doc.root();
    let child = root.children().next().unwrap();
    assert_eq!(child.kind, NodeKind::Pi);
    assert_eq!(child.name(), "solo-pi");
    assert!(child.content().is_empty());
}

#[test]
fn many_attributes_preserved() {
    let attrs: String = (1..=50).map(|i| format!(r#" a{i}="v{i}""#)).collect();
    let xml = format!("<r{attrs}/>");
    let doc = parse_str(&xml, &ParseOptions::default()).unwrap();
    let el = doc.root();
    assert_eq!(el.kind, NodeKind::Element);
    let collected: Vec<_> = el.attributes().collect();
    assert_eq!(collected.len(), 50);
    assert_eq!(collected[49].name(), "a50");
    assert_eq!(collected[49].value(), "v50");
}

#[test]
fn very_long_element_name() {
    let name = "a".repeat(500);
    let xml = format!("<{name}/>");
    let doc = parse_str(&xml, &ParseOptions::default()).unwrap();
    let root = doc.root();
    assert_eq!(root.kind, NodeKind::Element);
    assert_eq!(root.name(), name.as_str());
}

#[test]
fn very_long_text_content() {
    let content = "x".repeat(100_000);
    let xml = format!("<r>{content}</r>");
    let doc = parse_str(&xml, &ParseOptions::default()).unwrap();
    let root = doc.root();
    assert_eq!(root.children().next().unwrap().text_content().unwrap().len(), 100_000);
}

#[test]
fn deeply_nested_100_levels() {
    let open: String = (1..=100).map(|i| format!("<l{i}>")).collect();
    let close: String = (1..=100).rev().map(|i| format!("</l{i}>")).collect();
    let xml = format!("{open}deep{close}");
    parse_str(&xml, &ParseOptions::default())
        .expect("100 levels of nesting should not overflow the stack");
}

#[test]
fn unicode_element_names_via_utf8() {
    // Non-ASCII bytes in names: our parser accepts bytes >= 0xC0 as name-start.
    // This handles XML 1.1 style names and common practice even in XML 1.0.
    let xml = "<résumé>content</résumé>";
    parse_str(xml, &ParseOptions::default())
        .expect("UTF-8 element names should parse");
}

#[test]
fn mixed_content_preserved() {
    let xml = "<p>Hello <em>world</em>! How are <strong>you</strong>?</p>";
    let doc = parse_str(xml, &ParseOptions::default()).unwrap();
    let root = doc.root();
    let kids: Vec<_> = root.children().collect();
    // Text, Element, Text, Element, Text
    assert_eq!(kids.len(), 5);
    assert_eq!(kids[0].text_content(), Some("Hello "));
    assert_eq!(kids[1].kind, NodeKind::Element);
    assert_eq!(kids[1].name(), "em");
    assert_eq!(kids[2].text_content(), Some("! How are "));
    assert_eq!(kids[3].kind, NodeKind::Element);
    assert_eq!(kids[3].name(), "strong");
    assert_eq!(kids[4].text_content(), Some("?"));
}

#[test]
fn attribute_value_whitespace_is_normalized() {
    // XML §3.3.3 (CDATA-default normalization, applies regardless of
    // DTD typing): a literal `\t` / `\n` / `\r` in an attribute value
    // is rewritten to a single `#x20` space before the application
    // sees it.  To smuggle a real LF or tab through, the author must
    // use a character reference (`&#xA;`, `&#x9;`); that path is
    // exercised by `char_ref_in_attr_value` below.
    let xml = "<r a=\"line1\nline2\ttabbed\"/>";
    let doc = parse_str(xml, &ParseOptions::default()).unwrap();
    let el = doc.root();
    assert_eq!(el.attributes().next().unwrap().value(), "line1 line2 tabbed");
}

#[test]
fn attribute_value_char_ref_whitespace_is_preserved() {
    // Char-ref whitespace bypasses §3.3.3 normalization — the
    // standard way to preserve a literal tab or LF in an attribute
    // through the parse → DOM step.
    let xml = "<r a=\"line1&#xA;line2&#x9;tabbed\"/>";
    let doc = parse_str(xml, &ParseOptions::default()).unwrap();
    let el = doc.root();
    assert_eq!(el.attributes().next().unwrap().value(), "line1\nline2\ttabbed");
}

#[test]
fn attribute_value_entity_ref_whitespace_is_normalized() {
    // §3.3.3 step 2: an entity reference recursively applies the
    // algorithm to its replacement text — so literal whitespace
    // inside an entity's replacement still gets rewritten to space
    // when the reference appears in an attribute value (in contrast
    // to character-reference whitespace, which is preserved).
    let xml = "<!DOCTYPE r [<!ENTITY tabby \"a\tb\">]><r v=\"X&tabby;Y\"/>";
    let doc = parse_str(xml, &ParseOptions::default()).unwrap();
    let v = doc.root().attributes().next().unwrap().value().to_string();
    assert_eq!(v, "Xa bY");
}

#[test]
fn char_ref_in_attr_value() {
    let doc = parse_str(r#"<r a="&#x1D400;"/>"#, &ParseOptions::default()).unwrap();
    let el = doc.root();
    assert_eq!(el.attributes().next().unwrap().value(), "\u{1D400}");
}

#[test]
fn gt_unescaped_in_text_valid() {
    // '>' may appear unescaped in text content (only ']]>' is forbidden)
    let doc = parse_str("<r>a > b</r>", &ParseOptions::default()).unwrap();
    let root = doc.root();
    assert_eq!(root.children().next().unwrap().text_content(), Some("a > b"));
}

#[test]
fn cdata_gt_unescaped_in_serialize() {
    // After parse, '>' in text is serialized as '&gt;'
    let doc = parse_str("<r>a > b</r>", &ParseOptions::default()).unwrap();
    let out = serialize_to_string(&doc);
    assert!(out.contains("&gt;"), "serializer must escape '>' in text: {out}");
}
