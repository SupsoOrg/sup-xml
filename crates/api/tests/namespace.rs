/// Namespace resolution tests.
use sup_xml::{parse_ns_str, Document, NodeKind};

macro_rules! fixture {
    ($name:expr) => {
        include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/fixtures/", $name))
    };
}

/// Parse `src` with namespace resolution and return the document.  Callers
/// then call `doc.root()` to get the (lifetime-bound) root element.
fn parse_ns(src: &str) -> Document {
    parse_ns_str(src).unwrap()
}

// ── namespaces.xml fixture ────────────────────────────────────────────────────

#[test]
fn namespaces_fixture_resolves() {
    let doc = parse_ns_str(fixture!("namespaces.xml"))
        .expect("namespaces.xml should resolve without errors");
    let root = doc.root();
    assert_eq!(root.kind, NodeKind::Element);
    // Root has a default namespace
    let ns = root.namespace.get().expect("root should have default namespace");
    assert_eq!(ns.href(), "http://example.com/default");
}

#[test]
fn dc_prefix_resolved_in_fixture() {
    let doc = parse_ns_str(fixture!("namespaces.xml")).unwrap();
    let root = doc.root();

    // Match by (local_name, prefix) — `name()` is layout-dependent
    // (full QName on the lean build, local-only under c-abi), but
    // `local_name()` and the resolved `namespace.prefix()` are not.
    let dc_title = root.children()
        .find(|n| n.kind == NodeKind::Element
               && n.local_name() == "title"
               && n.namespace.get().and_then(|ns| ns.prefix()) == Some("dc"))
        .expect("dc:title child");

    let ns = dc_title.namespace.get().expect("dc:title must have namespace");
    assert_eq!(ns.href(), "http://purl.org/dc/elements/1.1/");
    assert_eq!(ns.prefix(), Some("dc"));
}

#[test]
fn xsi_attr_resolved_in_fixture() {
    let doc = parse_ns_str(fixture!("namespaces.xml")).unwrap();
    let root = doc.root();

    let schema_loc = root.attributes()
        .find(|a| a.local_name() == "schemaLocation"
               && a.namespace.get().and_then(|ns| ns.prefix()) == Some("xsi"))
        .expect("xsi:schemaLocation attribute");
    let ns = schema_loc.namespace.get().expect("xsi:schemaLocation must have namespace");
    assert_eq!(ns.href(), "http://www.w3.org/2001/XMLSchema-instance");
}

#[test]
fn xml_lang_resolved_in_fixture() {
    let doc = parse_ns_str(fixture!("namespaces.xml")).unwrap();
    let root = doc.root();

    // Find an item element, then find a name child with xml:lang
    let item = root.children()
        .find(|n| n.kind == NodeKind::Element && n.name() == "item")
        .expect("item element");
    let name_el = item.children()
        .find(|n| n.kind == NodeKind::Element && n.name() == "name")
        .expect("name element");
    let lang = name_el.attributes()
        .find(|a| a.local_name() == "lang"
               && a.namespace.get().and_then(|ns| ns.prefix()) == Some("xml"))
        .expect("xml:lang attribute");
    assert_eq!(
        lang.namespace.get().unwrap().href(),
        "http://www.w3.org/XML/1998/namespace"
    );
}

// ── scope and inheritance ─────────────────────────────────────────────────────

#[test]
fn sibling_elements_inherit_outer_scope() {
    let doc = parse_ns(r#"
        <root xmlns:a="http://a.com/">
            <a:one/>
            <a:two/>
        </root>
    "#);
    let el = doc.root();
    let children: Vec<_> = el.children()
        .filter(|n| n.is_element()).collect();
    assert_eq!(children.len(), 2);
    for child in children {
        let ns = child.namespace.get().unwrap();
        assert_eq!(ns.href(), "http://a.com/");
    }
}

#[test]
fn prefix_out_of_scope_is_error() {
    // Prefix declared only in a child, but used in the parent — should fail
    let result = parse_ns_str("<root><child xmlns:x=\"http://x.com/\"/><x:other/></root>");
    assert!(result.is_err(), "prefix used outside its declaring scope must be an error");
}

#[test]
fn multiple_prefixes_same_document() {
    let doc = parse_ns(r#"
        <root xmlns:a="http://a.com/" xmlns:b="http://b.com/">
            <a:foo/>
            <b:bar/>
        </root>
    "#);
    let el = doc.root();
    let a_el = el.children()
        .find(|n| n.kind == NodeKind::Element
               && n.local_name() == "foo"
               && n.namespace.get().and_then(|ns| ns.prefix()) == Some("a"))
        .unwrap();
    let b_el = el.children()
        .find(|n| n.kind == NodeKind::Element
               && n.local_name() == "bar"
               && n.namespace.get().and_then(|ns| ns.prefix()) == Some("b"))
        .unwrap();
    assert_eq!(a_el.namespace.get().unwrap().href(), "http://a.com/");
    assert_eq!(b_el.namespace.get().unwrap().href(), "http://b.com/");
}

#[test]
fn same_prefix_rebound_in_child() {
    // Per the XML Namespace spec §6.1, a namespace declaration applies to
    // the element it is written on (not just its children). So a:child with
    // xmlns:a="http://inner.com/" is itself in http://inner.com/.
    let doc = parse_ns(r#"
        <root xmlns:a="http://outer.com/">
            <a:outer-child/>
            <a:inner-child xmlns:a="http://inner.com/">
                <a:grandchild/>
            </a:inner-child>
        </root>
    "#);
    let el = doc.root();
    // First child uses the outer binding
    let outer_child = el.children()
        .find(|n| n.kind == NodeKind::Element
               && n.local_name() == "outer-child"
               && n.namespace.get().and_then(|ns| ns.prefix()) == Some("a"))
        .unwrap();
    assert_eq!(outer_child.namespace.get().unwrap().href(), "http://outer.com/");

    // Second child's xmlns:a declaration applies to itself
    let inner_child = el.children()
        .find(|n| n.kind == NodeKind::Element
               && n.local_name() == "inner-child"
               && n.namespace.get().and_then(|ns| ns.prefix()) == Some("a"))
        .unwrap();
    assert_eq!(inner_child.namespace.get().unwrap().href(), "http://inner.com/");

    // Grandchild also uses the inner binding
    let grandchild = inner_child.children()
        .find(|n| n.is_element())
        .unwrap();
    assert_eq!(grandchild.namespace.get().unwrap().href(), "http://inner.com/");
}

// ── xml: built-in prefix ─────────────────────────────────────────────────────

#[test]
fn xml_prefix_needs_no_declaration() {
    // xml: is always in scope without an explicit xmlns:xml declaration
    let doc = parse_ns(r#"<root xml:space="preserve"/>"#);
    let el = doc.root();
    let attr = el.attributes()
        .find(|a| a.local_name() == "space"
               && a.namespace.get().and_then(|ns| ns.prefix()) == Some("xml")).unwrap();
    assert_eq!(
        attr.namespace.get().unwrap().href(),
        "http://www.w3.org/XML/1998/namespace"
    );
}

// ── xmlns stays in attribute list ─────────────────────────────────────────────

#[test]
fn xmlns_decls_recoverable_after_resolution() {
    // After resolution, xmlns declarations should still be queryable.
    // libxml2 keeps them on the element's ns_def chain (NOT in the
    // attribute list); we follow that model under the c-abi build and
    // expose `ns_declarations()` for layout-agnostic querying.
    let doc = parse_ns(r#"<root xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:t/></root>"#);
    let el = doc.root();
    assert!(
        el.ns_declarations().any(|(prefix, href)|
            prefix == Some("dc") && href == "http://purl.org/dc/elements/1.1/"),
        "xmlns:dc should be recoverable via ns_declarations() after resolution"
    );
}

// ── edge cases ────────────────────────────────────────────────────────────────

#[test]
fn empty_default_namespace_undeclares() {
    let doc = parse_ns(r#"
        <root xmlns="http://example.com/">
            <child xmlns=""/>
        </root>
    "#);
    let el = doc.root();
    let child = el.children()
        .find(|n| n.is_element())
        .unwrap();
    assert!(
        child.namespace.get().is_none(),
        "xmlns='' should result in no namespace on the child"
    );
}

#[test]
fn no_namespace_document_still_resolves() {
    let doc = parse_ns("<catalog><book><title>XML Guide</title></book></catalog>");
    let el = doc.root();
    assert!(el.namespace.get().is_none());
    let book = el.children()
        .find(|n| n.is_element())
        .unwrap();
    assert!(book.namespace.get().is_none());
}
