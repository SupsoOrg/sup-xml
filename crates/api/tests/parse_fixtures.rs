use sup_xml::{parse_str, NodeKind, ParseOptions};

// Resolve fixture paths relative to the workspace root
macro_rules! fixture {
    ($name:expr) => {
        include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/fixtures/", $name))
    };
}

// ── simple.xml ────────────────────────────────────────────────────────────────

#[test]
fn simple_parses() {
    let doc = parse_str(fixture!("simple.xml"), &ParseOptions::default()).unwrap();
    let root = doc.root();
    assert_eq!(root.kind, NodeKind::Element);
    assert_eq!(root.name(), "catalog");
    assert_eq!(
        root.children().filter(|n| n.is_element()).count(),
        2
    );
}

#[test]
fn simple_book_attributes() {
    let doc = parse_str(fixture!("simple.xml"), &ParseOptions::default()).unwrap();
    let root = doc.root();
    assert_eq!(root.kind, NodeKind::Element);
    let book = root.children().find(|n| n.is_element()).unwrap();
    assert_eq!(book.kind, NodeKind::Element);
    assert_eq!(book.name(), "book");
    assert_eq!(
        book.attributes().find(|a| a.name() == "id").map(|a| a.value()),
        Some("bk101")
    );
}

// ── namespaces.xml ────────────────────────────────────────────────────────────

#[test]
fn namespaces_parses() {
    // `ns_declarations()` exposes xmlns declarations regardless of where the
    // build stores them — on the attribute list (lean) or the `ns_def` chain
    // (c-abi/libxml2 shape) — so this assertion holds under either build.
    let doc = parse_str(fixture!("namespaces.xml"), &ParseOptions::default()).unwrap();
    let root = doc.root();
    assert_eq!(root.kind, NodeKind::Element);
    assert_eq!(root.name(), "root");
    let prefixes: Vec<Option<&str>> = root.ns_declarations().map(|(p, _)| p).collect();
    assert!(prefixes.iter().any(|p| p.is_none()), "missing default namespace");
    assert!(prefixes.iter().any(|p| *p == Some("dc")), "missing dc namespace");
}

// ── attributes.xml ────────────────────────────────────────────────────────────

#[test]
fn attributes_parses() {
    let doc = parse_str(fixture!("attributes.xml"), &ParseOptions::default()).unwrap();
    assert_eq!(doc.root().kind, NodeKind::Element);
}

#[test]
fn entity_escapes_in_attributes() {
    let doc = parse_str(fixture!("attributes.xml"), &ParseOptions::default()).unwrap();
    let root = doc.root();
    assert_eq!(root.kind, NodeKind::Element);
    let escaped_el = root
        .children()
        .filter(|n| n.kind == NodeKind::Element && n.name() == "element")
        .find(|e| e.attributes().any(|a| a.name() == "escaped"))
        .expect("should find element with 'escaped' attribute");
    let val = escaped_el
        .attributes()
        .find(|a| a.name() == "escaped")
        .unwrap()
        .value();
    assert!(val.contains('<'), "< should be expanded from &lt;");
    assert!(val.contains('&'), "& should be expanded from &amp;");
    assert!(val.contains('"'), "\" should be expanded from &quot;");
}

// ── deep.xml ──────────────────────────────────────────────────────────────────

#[test]
fn deep_nesting_parses() {
    let doc = parse_str(fixture!("deep.xml"), &ParseOptions::default()).unwrap();
    // Walk from l1 → l20 (19 child traversals) and verify l20 contains text.
    let mut node = doc.root();
    assert_eq!(node.kind, NodeKind::Element);
    for depth in 1..=19 {
        assert_eq!(node.kind, NodeKind::Element, "expected element at depth {depth}");
        node = node
            .children()
            .find(|n| n.is_element())
            .unwrap_or_else(|| panic!("no child element at depth {depth}"));
    }
    // node is now l20 — it should have text content
    assert_eq!(node.kind, NodeKind::Element);
    assert_eq!(node.name(), "l20");
    assert!(node.children().any(|n| n.is_text()));
}

// ── cdata.xml ────────────────────────────────────────────────────────────────

#[test]
fn cdata_parses() {
    let doc = parse_str(fixture!("cdata.xml"), &ParseOptions::default()).unwrap();
    let root = doc.root();
    assert_eq!(root.kind, NodeKind::Element);

    // <script> child should contain a CData node with '<' characters
    let script = root
        .children()
        .find(|n| n.kind == NodeKind::Element && n.name() == "script")
        .expect("script element");
    let cdata_content = script
        .children()
        .find(|n| n.kind == NodeKind::CData)
        .map(|c| c.content())
        .expect("CDATA node inside script");
    assert!(cdata_content.contains('<'), "CDATA should preserve raw '<'");
}

#[test]
fn pi_inside_element_parses() {
    let doc = parse_str(fixture!("cdata.xml"), &ParseOptions::default()).unwrap();
    let root = doc.root();
    assert_eq!(root.kind, NodeKind::Element);
    let data = root
        .children()
        .find(|n| n.kind == NodeKind::Element && n.name() == "data")
        .expect("data element");
    let pi = data
        .children()
        .find(|n| n.kind == NodeKind::Pi)
        .expect("PI inside data");
    // In the arena DOM, a PI node's `name` holds the target.
    assert_eq!(pi.name(), "custom-pi");
}

// ── unicode.xml ───────────────────────────────────────────────────────────────

#[test]
fn unicode_parses() {
    let doc = parse_str(fixture!("unicode.xml"), &ParseOptions::default()).unwrap();
    let root = doc.root();
    assert_eq!(root.kind, NodeKind::Element);
    let cjk = root
        .children()
        .find(|n| n.kind == NodeKind::Element && n.name() == "cjk")
        .expect("cjk element");
    let text = cjk.children().next().unwrap().text_content().expect("text node");
    assert!(text.contains("日本語"), "should contain CJK characters");
}

#[test]
fn supplementary_char_ref_in_fixture() {
    let doc = parse_str(fixture!("unicode.xml"), &ParseOptions::default()).unwrap();
    let root = doc.root();
    assert_eq!(root.kind, NodeKind::Element);
    let math = root
        .children()
        .find(|n| n.kind == NodeKind::Element && n.name() == "math")
        .expect("math element");
    let text = math.children().next().unwrap().text_content().expect("text in math");
    // &#x1D400; → 𝐀 (U+1D400)
    assert!(text.contains('𝐀'), "supplementary char ref should expand correctly");
}

// ── CVE fixtures ──────────────────────────────────────────────────────────────

#[test]
fn billion_laughs_rejected() {
    // CVE-2003-1564: entity expansion must fail, not exhaust memory.
    // Today this is rejected because &lol9; is an undefined entity
    // (the DTD-declared general entities aren't expanded in this
    // configuration); once DTD entity expansion is enabled, the
    // byte-budget cap kicks in to catch this same fixture.
    let result = parse_str(
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/fixtures/cve/billion_laughs.xml"
        )),
        &ParseOptions::default(),
    );
    assert!(
        result.is_err(),
        "billion laughs must be rejected, not expanded"
    );
    let err = result.unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("undefined entity") || msg.contains("expansion"),
        "error message should mention entity: {msg}"
    );
}

macro_rules! asset {
    ($name:expr) => {
        include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/assets/xml/", $name))
    };
}

#[test]
fn flowingdata_parses() {
    parse_str(asset!("flowingdata.xml"), &ParseOptions::default())
        .expect("flowingdata.xml should parse cleanly");
}

#[test]
fn flowingdata_roundtrip() {
    use sup_xml::serialize_to_string;
    let doc1 = parse_str(asset!("flowingdata.xml"), &ParseOptions::default()).unwrap();
    let out = serialize_to_string(&doc1);
    parse_str(&out, &ParseOptions::default()).expect("flowingdata.xml roundtrip should parse");
}

// ── flat.xml ──────────────────────────────────────────────────────────────────
// 500 <item> siblings with 6 attributes each — wide/breadth parsing

#[test]
fn flat_parses() {
    let doc = parse_str(asset!("flat.xml"), &ParseOptions::default()).expect("flat.xml should parse");
    let root = doc.root();
    assert_eq!(root.kind, NodeKind::Element);
    assert_eq!(root.name(), "inventory");
    let item_count = root
        .children()
        .filter(|n| n.kind == NodeKind::Element && n.name() == "item")
        .count();
    assert_eq!(item_count, 500, "should have 500 item elements");
}

#[test]
fn flat_attributes_complete() {
    let doc = parse_str(asset!("flat.xml"), &ParseOptions::default()).unwrap();
    let root = doc.root();
    assert_eq!(root.kind, NodeKind::Element);
    let first = root
        .children()
        .find(|n| n.kind == NodeKind::Element && n.name() == "item")
        .expect("at least one item");
    for attr_name in &["id", "sku", "category", "status", "price", "qty"] {
        assert!(
            first.attributes().any(|a| a.name() == *attr_name),
            "item missing attribute '{attr_name}'"
        );
    }
}

// ── heavy_attrs.xml ───────────────────────────────────────────────────────────
// 100 <record> elements with 24 attributes each — attribute-dense parsing

#[test]
fn heavy_attrs_parses() {
    let doc = parse_str(asset!("heavy_attrs.xml"), &ParseOptions::default())
        .expect("heavy_attrs.xml should parse");
    let root = doc.root();
    assert_eq!(root.kind, NodeKind::Element);
    assert_eq!(root.name(), "records");
    let rec_count = root
        .children()
        .filter(|n| n.kind == NodeKind::Element && n.name() == "record")
        .count();
    assert_eq!(rec_count, 100, "should have 100 record elements");
}

#[test]
fn heavy_attrs_all_fields_present() {
    let doc = parse_str(asset!("heavy_attrs.xml"), &ParseOptions::default()).unwrap();
    let root = doc.root();
    assert_eq!(root.kind, NodeKind::Element);
    let record = root
        .children()
        .find(|n| n.kind == NodeKind::Element && n.name() == "record")
        .expect("at least one record");
    // Check all 20 numbered fields plus the 4 fixed attrs
    for n in 1..=20 {
        let name = format!("field{n:02}");
        assert!(
            record.attributes().any(|a| a.name() == name),
            "record missing attribute '{name}'"
        );
    }
    assert!(record.attributes().count() >= 24, "expected at least 24 attributes per record");
}

// ── ns_heavy.xml ──────────────────────────────────────────────────────────────
// 200 Atom feed entries with 6 namespace declarations — namespace-intensive parsing

#[test]
fn ns_heavy_parses() {
    let doc = parse_str(asset!("ns_heavy.xml"), &ParseOptions::default())
        .expect("ns_heavy.xml should parse");
    let root = doc.root();
    assert_eq!(root.kind, NodeKind::Element);
    assert_eq!(root.name(), "feed");
    let entry_count = root
        .children()
        .filter(|n| n.kind == NodeKind::Element && n.name() == "entry")
        .count();
    assert_eq!(entry_count, 200, "should have 200 entry elements");
}

#[test]
fn ns_heavy_root_namespaces() {
    let doc = parse_str(asset!("ns_heavy.xml"), &ParseOptions::default()).unwrap();
    let root = doc.root();
    assert_eq!(root.kind, NodeKind::Element);
    let prefixes: Vec<Option<&str>> = root.ns_declarations().map(|(p, _)| p).collect();
    assert!(prefixes.iter().any(|p| p.is_none()), "missing default namespace");
    assert!(prefixes.iter().any(|p| *p == Some("dc")), "missing dc namespace");
    assert!(prefixes.iter().any(|p| *p == Some("media")), "missing media namespace");
}

#[test]
fn ns_heavy_entry_children() {
    let doc = parse_str(asset!("ns_heavy.xml"), &ParseOptions::default()).unwrap();
    let root = doc.root();
    assert_eq!(root.kind, NodeKind::Element);
    let entry = root
        .children()
        .find(|n| n.kind == NodeKind::Element && n.name() == "entry")
        .expect("at least one entry");
    let child_names: Vec<&str> = entry
        .children()
        .filter(|n| n.kind == NodeKind::Element)
        .map(|n| n.name())
        .collect();
    for expected in &["id", "title", "author", "updated"] {
        assert!(child_names.iter().any(|n| n == expected), "entry missing child <{expected}>");
    }
}

// ── mixed_content.xml ─────────────────────────────────────────────────────────
// 50 sections with paragraph mixed content (text + inline elements)

#[test]
fn mixed_content_parses() {
    let doc = parse_str(asset!("mixed_content.xml"), &ParseOptions::default())
        .expect("mixed_content.xml should parse");
    let root = doc.root();
    assert_eq!(root.kind, NodeKind::Element);
    assert_eq!(root.name(), "article");
    let section_count = root
        .children()
        .filter(|n| n.kind == NodeKind::Element && n.name() == "section")
        .count();
    assert_eq!(section_count, 50, "should have 50 section elements");
}

#[test]
fn mixed_content_paragraphs_have_inline_elements() {
    let doc = parse_str(asset!("mixed_content.xml"), &ParseOptions::default()).unwrap();
    let root = doc.root();
    assert_eq!(root.kind, NodeKind::Element);
    let section = root
        .children()
        .find(|n| n.kind == NodeKind::Element && n.name() == "section")
        .expect("at least one section");
    let inline_tags = ["em", "strong", "code", "abbr", "cite"];
    let has_inline = section
        .children()
        .filter(|n| n.kind == NodeKind::Element && n.name() == "p")
        .any(|p| {
            p.children()
                .any(|n| n.kind == NodeKind::Element && inline_tags.contains(&n.name()))
        });
    let any_p = section
        .children()
        .any(|n| n.kind == NodeKind::Element && n.name() == "p");
    assert!(any_p, "section should contain <p> elements");
    assert!(has_inline, "paragraphs should contain inline elements like <em>, <strong>, etc.");
}

#[test]
fn mixed_content_text_preserved() {
    let doc = parse_str(asset!("mixed_content.xml"), &ParseOptions::default()).unwrap();
    let root = doc.root();
    assert_eq!(root.kind, NodeKind::Element);
    let section = root
        .children()
        .find(|n| n.kind == NodeKind::Element && n.name() == "section")
        .unwrap();
    let para = section
        .children()
        .find(|n| n.kind == NodeKind::Element && n.name() == "p")
        .unwrap();
    let has_text = para.children().any(|n| n.kind == NodeKind::Text);
    assert!(has_text, "<p> elements should contain text nodes");
}
