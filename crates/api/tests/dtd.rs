/// DTD entity expansion tests.
use sup_xml::{parse_str, NodeKind, ParseOptions};

macro_rules! fixture {
    ($name:expr) => {
        include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/fixtures/", $name))
    };
}

// ── basic entity expansion ────────────────────────────────────────────────────

#[test]
fn simple_entity_expands() {
    let xml = r#"<!DOCTYPE r [<!ENTITY greeting "Hello, World!">]><r>&greeting;</r>"#;
    let doc = parse_str(xml, &ParseOptions::default()).unwrap();
    let root = doc.root();
    assert_eq!(root.kind, NodeKind::Element);
    assert_eq!(root.children().next().unwrap().text_content(), Some("Hello, World!"));
}

#[test]
fn entity_in_attribute_expands() {
    let xml = r#"<!DOCTYPE r [<!ENTITY who "Alice">]><r name="&who;"/>"#;
    let doc = parse_str(xml, &ParseOptions::default()).unwrap();
    let el = doc.root();
    assert_eq!(el.kind, NodeKind::Element);
    assert_eq!(el.attributes().next().unwrap().value(), "Alice");
}

#[test]
fn multiple_entities_expand() {
    let xml = r#"<!DOCTYPE r [
        <!ENTITY a "AAA">
        <!ENTITY b "BBB">
    ]><r>&a;&b;</r>"#;
    let doc = parse_str(xml, &ParseOptions::default()).unwrap();
    let root = doc.root();
    let text: String = root.children()
        .filter_map(|n| n.text_content())
        .collect();
    assert_eq!(text, "AAABBB");
}

#[test]
fn entity_used_multiple_times() {
    let xml = r#"<!DOCTYPE r [<!ENTITY x "X">]><r>&x;&x;&x;</r>"#;
    let doc = parse_str(xml, &ParseOptions::default()).unwrap();
    let root = doc.root();
    let text: String = root.children()
        .filter_map(|n| n.text_content())
        .collect();
    assert_eq!(text, "XXX");
}

#[test]
fn entity_chain_expands() {
    // b references a — two-level expansion
    let xml = r#"<!DOCTYPE r [
        <!ENTITY a "inner">
        <!ENTITY b "before-&a;-after">
    ]><r>&b;</r>"#;
    let doc = parse_str(xml, &ParseOptions::default()).unwrap();
    let root = doc.root();
    let text: String = root.children()
        .filter_map(|n| n.text_content())
        .collect();
    assert_eq!(text, "before-inner-after");
}

// ── undefined entity ──────────────────────────────────────────────────────────

#[test]
fn undefined_entity_is_error() {
    let xml = "<r>&undefined;</r>";
    assert!(parse_str(xml, &ParseOptions::default()).is_err(), "undefined entity must be an error");
}

// ── recursion detection ───────────────────────────────────────────────────────

#[test]
fn direct_recursive_entity_rejected() {
    let xml = r#"<!DOCTYPE r [<!ENTITY x "&x;">]><r>&x;</r>"#;
    assert!(parse_str(xml, &ParseOptions::default()).is_err(), "direct entity recursion must be rejected");
}

#[test]
fn indirect_recursive_entity_rejected() {
    let xml = r#"<!DOCTYPE r [
        <!ENTITY a "&b;">
        <!ENTITY b "&a;">
    ]><r>&a;</r>"#;
    assert!(parse_str(xml, &ParseOptions::default()).is_err(), "indirect entity recursion must be rejected");
}

// ── billion laughs (CVE-2003-1564) ───────────────────────────────────────────

#[test]
fn billion_laughs_hits_budget() {
    let xml = fixture!("cve/billion_laughs.xml");
    let err = parse_str(xml, &ParseOptions::default()).expect_err("billion laughs must be rejected");
    let msg = err.message.to_lowercase();
    assert!(
        msg.contains("budget") || msg.contains("expansion") || msg.contains("limit") || msg.contains("bytes"),
        "error should mention budget/expansion/limit, got: {}", err.message
    );
}

#[test]
fn custom_budget_enforced() {
    // A deeply-expanded entity that's small enough with default budget but over 100 bytes.
    let xml = r#"<!DOCTYPE r [
        <!ENTITY a "AAAAAAAAAA">
        <!ENTITY b "&a;&a;&a;&a;&a;&a;&a;&a;&a;&a;">
        <!ENTITY c "&b;&b;&b;&b;&b;&b;&b;&b;&b;&b;">
    ]><r>&c;</r>"#;
    // c → 10 × b, b → 10 × a, a = 10 bytes → 10 × 10 × 10 = 1000 bytes

    // default budget (1 MB) accepts it
    parse_str(xml, &ParseOptions::default()).expect("1000-byte expansion should fit default budget");

    // budget of 500 bytes rejects it
    let opts = ParseOptions { max_entity_expansion_bytes: 500, ..Default::default() };
    let err = parse_str(xml, &opts).expect_err("500-byte budget should reject 1000-byte expansion");
    let msg = err.message.to_lowercase();
    assert!(msg.contains("budget") || msg.contains("expansion") || msg.contains("limit") || msg.contains("bytes"),
        "error should mention limit: {}", err.message);
}

// ── depth limit ───────────────────────────────────────────────────────────────

#[test]
fn depth_limit_default_allows_256() {
    let open: String = (1..=256).map(|i| format!("<l{i}>")).collect();
    let close: String = (1..=256).rev().map(|i| format!("</l{i}>")).collect();
    let xml = format!("{open}ok{close}");
    parse_str(&xml, &ParseOptions::default()).expect("256 levels must be accepted with default depth limit");
}

#[test]
fn depth_limit_rejects_257() {
    let open: String = (1..=257).map(|i| format!("<l{i}>")).collect();
    let close: String = (1..=257).rev().map(|i| format!("</l{i}>")).collect();
    let xml = format!("{open}deep{close}");
    assert!(parse_str(&xml, &ParseOptions::default()).is_err(), "257 levels must be rejected with default depth limit");
}

#[test]
fn custom_depth_limit_enforced() {
    let open: String = (1..=11).map(|i| format!("<l{i}>")).collect();
    let close: String = (1..=11).rev().map(|i| format!("</l{i}>")).collect();
    let xml = format!("{open}deep{close}");

    // limit 10 → reject 11
    let opts = ParseOptions { max_element_depth: 10, ..Default::default() };
    assert!(parse_str(&xml, &opts).is_err(), "depth 11 must be rejected when limit is 10");

    // limit 20 → accept 11
    let opts = ParseOptions { max_element_depth: 20, ..Default::default() };
    parse_str(&xml, &opts).expect("depth 11 must be accepted when limit is 20");
}

// ── predefined entities ───────────────────────────────────────────────────────

#[test]
fn predefined_entities_work_without_doctype() {
    let doc = parse_str("<r>&amp;&lt;&gt;&apos;&quot;</r>", &ParseOptions::default()).unwrap();
    let root = doc.root();
    let text: String = root.children()
        .filter_map(|n| n.text_content())
        .collect();
    assert_eq!(text, r#"&<>'""#);
}

// ── external entity blocked ───────────────────────────────────────────────────

#[test]
fn external_entity_blocked_by_default() {
    // SYSTEM entities must be silently skipped (not loaded), so no error on declaration,
    // but referencing the entity should error because it was never defined.
    let xml = r#"<!DOCTYPE r [<!ENTITY ext SYSTEM "file:///etc/passwd">]><r>&ext;</r>"#;
    assert!(parse_str(xml, &ParseOptions::default()).is_err(), "external entity reference must error when loading is disabled");
}
