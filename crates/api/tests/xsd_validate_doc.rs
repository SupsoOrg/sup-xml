//! `validate_doc` shadow tests — every fixture is validated through
//! both `validate_str` (streaming) and `validate_doc` (DOM walker),
//! and the resulting issues are compared.  If the two paths ever
//! diverge on the same input, one of these tests fails.
//!
//! Comparison is by:
//!   - issue count
//!   - per-issue: `kind` + `message` + `path`
//!
//! Line / column intentionally excluded — `validate_doc` doesn't
//! have source byte offsets to anchor diagnostics, by design.

#![cfg(feature = "xsd")]

use sup_xml::{parse_str, ParseOptions};
use sup_xml::xsd::{Schema, ValidationError, ValidationIssue};

/// Run both validators and assert they produce equivalent issue
/// lists (same count, same kinds, same paths, same messages).  The
/// instance is parsed namespace-blind (`ParseOptions::default()`).
fn assert_paths_agree(schema_xml: &str, instance_xml: &str) {
    assert_paths_agree_with(schema_xml, instance_xml, &ParseOptions::default());
}

/// As [`assert_paths_agree`], but parses the instance with full
/// namespace awareness.  Under the c-abi tree layout this routes
/// `xmlns` declarations onto the `ns_def` chain rather than the
/// attribute list, so the DOM walker must surface them from there for
/// its namespace view to match the streaming parser's.  Guards the
/// build-independence of `validate_doc`'s namespace resolution.
fn assert_paths_agree_ns(schema_xml: &str, instance_xml: &str) {
    let opts = ParseOptions { namespace_aware: true, ..ParseOptions::default() };
    assert_paths_agree_with(schema_xml, instance_xml, &opts);
}

fn assert_paths_agree_with(schema_xml: &str, instance_xml: &str, opts: &ParseOptions) {
    let schema = Schema::compile_str(schema_xml).expect("schema must compile");
    let res_str = schema.validate_str(instance_xml);
    let doc = parse_str(instance_xml, opts).expect("parse instance");
    let res_doc = schema.validate_doc(&doc);

    let str_issues = issues(&res_str);
    let doc_issues = issues(&res_doc);

    assert_eq!(str_issues.len(), doc_issues.len(),
        "issue count mismatch: validate_str={} validate_doc={}\n\
         validate_str: {:#?}\n\
         validate_doc: {:#?}",
        str_issues.len(), doc_issues.len(), str_issues, doc_issues,
    );
    for (a, b) in str_issues.iter().zip(doc_issues.iter()) {
        assert_eq!(format!("{:?}", a.kind), format!("{:?}", b.kind),
            "issue kind differs:\n  str: {a:?}\n  doc: {b:?}");
        assert_eq!(a.path, b.path,
            "issue path differs:\n  str: {a:?}\n  doc: {b:?}");
        assert_eq!(a.message, b.message,
            "issue message differs:\n  str: {a:?}\n  doc: {b:?}");
    }
}

fn issues(r: &Result<(), ValidationError>) -> Vec<ValidationIssue> {
    match r {
        Ok(())  => Vec::new(),
        Err(e)  => e.issues.clone(),
    }
}

// ── valid instances: both paths return Ok ────────────────────────

#[test]
fn valid_minimal_typed_element() {
    let xsd = r#"<?xml version="1.0"?>
        <xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema">
          <xs:element name="port" type="xs:int"/>
        </xs:schema>"#;
    assert_paths_agree(xsd, "<port>8080</port>");
}

#[test]
fn valid_nested_sequence_with_attributes() {
    let xsd = r#"<?xml version="1.0"?>
        <xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema">
          <xs:element name="book">
            <xs:complexType>
              <xs:sequence>
                <xs:element name="title" type="xs:string"/>
                <xs:element name="year"  type="xs:int"/>
              </xs:sequence>
              <xs:attribute name="isbn" type="xs:string" use="required"/>
            </xs:complexType>
          </xs:element>
        </xs:schema>"#;
    assert_paths_agree(xsd,
        r#"<book isbn="0-13-110362-8"><title>The C Book</title><year>1988</year></book>"#);
}

#[test]
fn valid_namespaced_element_with_qualified_form() {
    let xsd = r#"<?xml version="1.0"?>
        <xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema"
                   targetNamespace="urn:demo" xmlns="urn:demo"
                   elementFormDefault="qualified">
          <xs:element name="root">
            <xs:complexType>
              <xs:sequence>
                <xs:element name="child" type="xs:string"/>
              </xs:sequence>
            </xs:complexType>
          </xs:element>
        </xs:schema>"#;
    assert_paths_agree(xsd,
        r#"<root xmlns="urn:demo"><child>hi</child></root>"#);
}

#[test]
fn valid_with_comments_and_pis_between_elements() {
    let xsd = r#"<?xml version="1.0"?>
        <xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema">
          <xs:element name="r">
            <xs:complexType>
              <xs:sequence>
                <xs:element name="a" type="xs:string"/>
                <xs:element name="b" type="xs:string"/>
              </xs:sequence>
            </xs:complexType>
          </xs:element>
        </xs:schema>"#;
    // Comments / PIs interleaved with element content — both paths
    // must skip them identically.
    assert_paths_agree(xsd,
        "<r><!-- pre-a --><a>x</a><?ignore me?><b>y</b><!-- after --></r>");
}

#[test]
fn valid_with_cdata_text_content() {
    let xsd = r#"<?xml version="1.0"?>
        <xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema">
          <xs:element name="note" type="xs:string"/>
        </xs:schema>"#;
    assert_paths_agree(xsd, "<note><![CDATA[raw & < > stuff]]></note>");
}

#[test]
fn valid_with_mixed_attribute_orderings() {
    let xsd = r#"<?xml version="1.0"?>
        <xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema">
          <xs:element name="r">
            <xs:complexType>
              <xs:attribute name="a" type="xs:string"/>
              <xs:attribute name="b" type="xs:string"/>
              <xs:attribute name="c" type="xs:string"/>
            </xs:complexType>
          </xs:element>
        </xs:schema>"#;
    assert_paths_agree(xsd, r#"<r a="1" b="2" c="3"/>"#);
}

// ── invalid instances: both paths return the same error ─────────

#[test]
fn invalid_wrong_element_type_produces_same_diagnostic() {
    let xsd = r#"<?xml version="1.0"?>
        <xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema">
          <xs:element name="port" type="xs:int"/>
        </xs:schema>"#;
    assert_paths_agree(xsd, "<port>not a number</port>");
}

#[test]
fn invalid_missing_required_attribute() {
    let xsd = r#"<?xml version="1.0"?>
        <xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema">
          <xs:element name="r">
            <xs:complexType>
              <xs:attribute name="required-attr" type="xs:string" use="required"/>
            </xs:complexType>
          </xs:element>
        </xs:schema>"#;
    assert_paths_agree(xsd, "<r/>");
}

#[test]
fn invalid_unexpected_element_in_sequence() {
    let xsd = r#"<?xml version="1.0"?>
        <xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema">
          <xs:element name="r">
            <xs:complexType>
              <xs:sequence>
                <xs:element name="a" type="xs:string"/>
                <xs:element name="b" type="xs:string"/>
              </xs:sequence>
            </xs:complexType>
          </xs:element>
        </xs:schema>"#;
    // <c> isn't in the model — the kind and path should match across
    // both paths.
    assert_paths_agree(xsd, "<r><a>1</a><c>2</c></r>");
}

#[test]
fn invalid_sibling_index_path_reflects_third_book() {
    let xsd = r#"<?xml version="1.0"?>
        <xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema">
          <xs:element name="catalog">
            <xs:complexType>
              <xs:sequence>
                <xs:element name="book" maxOccurs="unbounded">
                  <xs:complexType>
                    <xs:attribute name="isbn" type="xs:string" use="required"/>
                  </xs:complexType>
                </xs:element>
              </xs:sequence>
            </xs:complexType>
          </xs:element>
        </xs:schema>"#;
    // The third <book> is missing isbn — the path should read
    // `/catalog/book[3]` from both paths.
    assert_paths_agree(xsd, r#"<catalog>
        <book isbn="A"/>
        <book isbn="B"/>
        <book/>
    </catalog>"#);
}

#[test]
fn invalid_type_inside_namespace_qualified_doc() {
    let xsd = r#"<?xml version="1.0"?>
        <xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema"
                   targetNamespace="urn:demo" xmlns="urn:demo"
                   elementFormDefault="qualified">
          <xs:element name="root">
            <xs:complexType>
              <xs:sequence>
                <xs:element name="port" type="xs:int"/>
              </xs:sequence>
            </xs:complexType>
          </xs:element>
        </xs:schema>"#;
    assert_paths_agree(xsd,
        r#"<root xmlns="urn:demo"><port>oops</port></root>"#);
}

#[test]
fn valid_namespaced_doc_parsed_namespace_aware() {
    // Same shape as `valid_namespaced_element_with_qualified_form`, but
    // the instance is parsed namespace-aware so its `xmlns="urn:demo"`
    // declaration lives on `ns_def` (c-abi) instead of the attribute
    // list.  `validate_doc` must still resolve the default namespace and
    // match the qualified schema element — no spurious "unexpected
    // element" issues.
    let xsd = r#"<?xml version="1.0"?>
        <xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema"
                   targetNamespace="urn:demo" xmlns="urn:demo"
                   elementFormDefault="qualified">
          <xs:element name="root">
            <xs:complexType>
              <xs:sequence>
                <xs:element name="child" type="xs:string"/>
              </xs:sequence>
            </xs:complexType>
          </xs:element>
        </xs:schema>"#;
    assert_paths_agree_ns(xsd,
        r#"<root xmlns="urn:demo"><child>hi</child></root>"#);
}

#[test]
fn invalid_namespaced_doc_parsed_namespace_aware() {
    // The type error must surface identically whether the default
    // namespace declaration sits in the attribute list or on `ns_def`.
    let xsd = r#"<?xml version="1.0"?>
        <xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema"
                   targetNamespace="urn:demo" xmlns="urn:demo"
                   elementFormDefault="qualified">
          <xs:element name="root">
            <xs:complexType>
              <xs:sequence>
                <xs:element name="port" type="xs:int"/>
              </xs:sequence>
            </xs:complexType>
          </xs:element>
        </xs:schema>"#;
    assert_paths_agree_ns(xsd,
        r#"<root xmlns="urn:demo"><port>oops</port></root>"#);
}

#[test]
fn invalid_with_multiple_independent_issues() {
    let xsd = r#"<?xml version="1.0"?>
        <xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema">
          <xs:element name="r">
            <xs:complexType>
              <xs:sequence>
                <xs:element name="a" type="xs:int"/>
                <xs:element name="b" type="xs:int"/>
              </xs:sequence>
              <xs:attribute name="needs" type="xs:string" use="required"/>
            </xs:complexType>
          </xs:element>
        </xs:schema>"#;
    // Missing attribute + bad <a> type + bad <b> type — three
    // distinct issues; both paths must surface the same set.
    assert_paths_agree(xsd, "<r><a>x</a><b>y</b></r>");
}

// ── DTD default attribute injection survives the round-trip ──────

#[test]
fn dtd_default_attribute_is_seen_by_both_paths() {
    // The instance declares a DTD with a default attribute.  The
    // parser injects defaults at DOM-construction time, so the
    // walker sees the synthesised attribute the same way the
    // streaming reader emits it.
    let xsd = r#"<?xml version="1.0"?>
        <xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema">
          <xs:element name="r">
            <xs:complexType>
              <xs:attribute name="flavour" type="xs:string"/>
            </xs:complexType>
          </xs:element>
        </xs:schema>"#;
    let instance = r#"<?xml version="1.0"?>
        <!DOCTYPE r [<!ATTLIST r flavour CDATA "vanilla">]>
        <r/>"#;
    assert_paths_agree(xsd, instance);
}

// ── deeply nested input ──────────────────────────────────────────

#[test]
fn deeply_nested_valid_doc() {
    // Verifies the walker's iterative traversal doesn't stack-overflow
    // and produces identical results to the streaming path.
    let xsd = r#"<?xml version="1.0"?>
        <xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema">
          <xs:element name="nest">
            <xs:complexType>
              <xs:sequence>
                <xs:element name="nest" minOccurs="0"/>
              </xs:sequence>
            </xs:complexType>
          </xs:element>
        </xs:schema>"#;
    // 50 levels of <nest>.
    let mut s = String::new();
    for _ in 0..50 { s.push_str("<nest>"); }
    for _ in 0..50 { s.push_str("</nest>"); }
    assert_paths_agree(xsd, &s);
}
