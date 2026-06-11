//! End-to-end schema-aware processing tests.
//!
//! Exercises the full chain: `xsl:import-schema` loads a schema, the
//! source document is validated against it (the post-schema-validation
//! infoset), source nodes carry their governing type, and `data()`
//! atomizes a schema-typed node to its *typed* value — so
//! `instance of` and the typed string form reflect the real type
//! rather than xs:untypedAtomic.
//!
//! These run only under the `xsd` feature (the gate for schema-aware
//! processing); without it the chain compiles out and source nodes
//! stay untyped.

#![cfg(feature = "xsd")]

use sup_xml_core::{parse_str, ParseOptions};
use sup_xml_xslt::loader::InMemoryLoader;
use sup_xml_xslt::Stylesheet;

const SCHEMA: &str = r#"<?xml version="1.0"?>
<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema"
           targetNamespace="urn:test"
           xmlns="urn:test"
           elementFormDefault="qualified">
  <xs:element name="root">
    <xs:complexType>
      <xs:sequence>
        <xs:element name="n" type="xs:integer"/>
        <xs:element name="s" type="xs:string"/>
      </xs:sequence>
    </xs:complexType>
  </xs:element>
</xs:schema>"#;

/// A source whose `<n>` is schema-typed `xs:integer` atomizes through
/// `data()` to a typed integer: its canonical form drops the leading
/// zero, it *is* an instance of xs:integer, and it is *not* an
/// instance of xs:string.  An untyped (non-schema) run would keep the
/// lexical "042" and report it as a string.
#[test]
fn data_of_schema_typed_source_node_is_typed() {
    let xsl = r#"<xsl:stylesheet version="3.0"
            xmlns:xsl="http://www.w3.org/1999/XSL/Transform"
            xmlns:xs="http://www.w3.org/2001/XMLSchema">
        <xsl:import-schema namespace="urn:test" schema-location="test.xsd"/>
        <xsl:template match="/">
            <out canonical="{data(/*/*[1])}"
                 is-integer="{data(/*/*[1]) instance of xs:integer}"
                 is-string="{data(/*/*[1]) instance of xs:string}"/>
        </xsl:template>
    </xsl:stylesheet>"#;

    let loader = InMemoryLoader::new().with("test.xsd", SCHEMA);
    let style = Stylesheet::compile_str_with_loader(xsl, &loader, None)
        .expect("stylesheet with import-schema should compile");

    let mut opts = ParseOptions::default();
    opts.namespace_aware = true;
    let src = parse_str(
        r#"<root xmlns="urn:test"><n>042</n><s>hi</s></root>"#, &opts,
    ).unwrap();

    let out = style.apply_with_loader(&src, &loader, None)
        .expect("apply should succeed")
        .to_string()
        .unwrap();

    assert!(out.contains(r#"canonical="42""#),
        "typed integer should atomize to its canonical form, got: {out}");
    assert!(out.contains(r#"is-integer="true""#),
        "typed value should be an instance of xs:integer, got: {out}");
    assert!(out.contains(r#"is-string="false""#),
        "an xs:integer-typed value is NOT an instance of xs:string, got: {out}");
}

/// A constructed element carrying `xsl:type="xs:integer"` atomizes to a
/// typed integer: canonical "3", an instance of xs:integer but not
/// xs:string.  No schema import needed — the type is built-in.
#[test]
fn data_of_constructed_typed_element_is_typed() {
    let xsl = r#"<xsl:stylesheet version="2.0"
            xmlns:xsl="http://www.w3.org/1999/XSL/Transform"
            xmlns:xs="http://www.w3.org/2001/XMLSchema">
        <xsl:variable name="t"><e xsl:type="xs:integer">003</e></xsl:variable>
        <xsl:template match="/">
            <out canonical="{data($t/e)}"
                 is-int="{data($t/e) instance of xs:integer}"
                 is-str="{data($t/e) instance of xs:string}"/>
        </xsl:template>
    </xsl:stylesheet>"#;

    let style = Stylesheet::compile_str(xsl).expect("compile");
    let mut opts = ParseOptions::default();
    opts.namespace_aware = true;
    let src = parse_str("<doc/>", &opts).unwrap();
    let out = style.apply(&src).expect("apply").to_string().unwrap();

    assert!(out.contains(r#"canonical="3""#),
        "constructed xs:integer should atomize to canonical '3', got: {out}");
    assert!(out.contains(r#"is-int="true""#),
        "constructed xs:integer-typed value is an instance of xs:integer, got: {out}");
    assert!(out.contains(r#"is-str="false""#),
        "constructed xs:integer is NOT an instance of xs:string, got: {out}");
}
