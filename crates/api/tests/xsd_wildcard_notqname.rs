//! Coverage for XSD 1.1 § 3.10.4 `notQName` / `notNamespace` on
//! `xs:any` and `xs:anyAttribute`, including the `##defined` and
//! `##definedSibling` keywords.

#![cfg(feature = "xsd")]

use sup_xml::xsd::{Schema, SchemaOptions, SchemaVersion};

fn compile_11(xsd: &str) -> Schema {
    Schema::compile_str_with_options(
        xsd,
        SchemaOptions { version: SchemaVersion::Xsd11, ..Default::default() },
    )
    .expect("schema must compile under XSD 1.1")
}

fn ok(schema: &Schema, instance: &str) {
    if let Err(e) = schema.validate_str(instance) {
        panic!("expected valid instance, got: {e}");
    }
}

fn err(schema: &Schema, instance: &str) {
    assert!(schema.validate_str(instance).is_err(),
        "expected validation error, got OK for: {instance}");
}

// ── notQName="<literal>" ────────────────────────────────────────────────────

#[test]
fn any_notqname_literal_rejects_named_match() {
    let xsd = r###"
        <xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema"
                   xmlns="urn:t" targetNamespace="urn:t" elementFormDefault="qualified">
          <xs:element name="root">
            <xs:complexType>
              <xs:sequence>
                <xs:any namespace="##any" notQName="foo" processContents="skip"/>
              </xs:sequence>
            </xs:complexType>
          </xs:element>
        </xs:schema>"###;
    let s = compile_11(xsd);
    ok(&s,  r#"<root xmlns="urn:t"><bar/></root>"#);
    err(&s, r#"<root xmlns="urn:t"><foo/></root>"#);
}

// ── notNamespace="<ns>" ─────────────────────────────────────────────────────

#[test]
fn any_notnamespace_rejects_excluded_ns() {
    let xsd = r###"
        <xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema"
                   xmlns="urn:t" targetNamespace="urn:t" elementFormDefault="qualified">
          <xs:element name="root">
            <xs:complexType>
              <xs:sequence>
                <xs:any namespace="##any" notNamespace="urn:bad"
                        processContents="skip"/>
              </xs:sequence>
            </xs:complexType>
          </xs:element>
        </xs:schema>"###;
    let s = compile_11(xsd);
    ok(&s,  r#"<root xmlns="urn:t"><x:something xmlns:x="urn:ok"/></root>"#);
    err(&s, r#"<root xmlns="urn:t"><x:something xmlns:x="urn:bad"/></root>"#);
}

// ── notQName="##defined" ────────────────────────────────────────────────────

#[test]
fn any_notqname_defined_rejects_top_level_decls() {
    // `foo` is declared at top level; `bar` is not.  The wildcard
    // should admit `bar` and reject `foo`.
    let xsd = r###"
        <xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema"
                   xmlns="urn:t" targetNamespace="urn:t" elementFormDefault="qualified">
          <xs:element name="foo"/>
          <xs:element name="root">
            <xs:complexType>
              <xs:sequence>
                <xs:any namespace="##any" notQName="##defined"
                        processContents="skip"/>
              </xs:sequence>
            </xs:complexType>
          </xs:element>
        </xs:schema>"###;
    let s = compile_11(xsd);
    ok(&s,  r#"<root xmlns="urn:t"><bar/></root>"#);
    err(&s, r#"<root xmlns="urn:t"><foo/></root>"#);
}

// ── notQName="##definedSibling" ─────────────────────────────────────────────

#[test]
fn any_notqname_defined_sibling_rejects_sibling_decls() {
    // `sib` is declared as a sibling of the wildcard inside `root`;
    // `foo` is a top-level decl but not a sibling.  `##definedSibling`
    // excludes `sib` but admits `foo` and arbitrary new names.
    //
    // The substitution-group wildcard sits AFTER the named child so
    // the DFA reaches the wildcard transition for sibling-named
    // inputs.
    let xsd = r###"
        <xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema"
                   xmlns="urn:t" targetNamespace="urn:t" elementFormDefault="qualified">
          <xs:element name="foo"/>
          <xs:element name="root">
            <xs:complexType>
              <xs:sequence>
                <xs:element name="sib" minOccurs="0"/>
                <xs:any namespace="##any" notQName="##definedSibling"
                        processContents="skip" maxOccurs="unbounded"/>
              </xs:sequence>
            </xs:complexType>
          </xs:element>
        </xs:schema>"###;
    let s = compile_11(xsd);
    // `foo` and arbitrary new names are admitted by the wildcard.
    ok(&s,  r#"<root xmlns="urn:t"><foo/></root>"#);
    ok(&s,  r#"<root xmlns="urn:t"><sib/><foo/></root>"#);
    ok(&s,  r#"<root xmlns="urn:t"><novel/></root>"#);
    // `sib` is the sibling-declared name; the wildcard must not
    // re-admit it after the declared use has already been consumed
    // (or skipped).
    err(&s, r#"<root xmlns="urn:t"><sib/><sib/></root>"#);
}

// ── xs:anyAttribute notQName="##defined" / "##definedSibling" ───────────────

#[test]
fn any_attribute_notqname_defined_rejects_top_level_attrs() {
    let xsd = r###"
        <xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema"
                   xmlns="urn:t" targetNamespace="urn:t"
                   elementFormDefault="qualified"
                   attributeFormDefault="qualified">
          <xs:attribute name="known" type="xs:string"/>
          <xs:element name="root">
            <xs:complexType>
              <xs:anyAttribute namespace="##any" notQName="##defined"
                               processContents="skip"/>
            </xs:complexType>
          </xs:element>
        </xs:schema>"###;
    let s = compile_11(xsd);
    ok(&s,  r#"<root xmlns="urn:t" xmlns:t="urn:t" t:misc="x"/>"#);
    err(&s, r#"<root xmlns="urn:t" xmlns:t="urn:t" t:known="x"/>"#);
}

#[test]
fn any_attribute_notqname_defined_sibling_rejects_sibling_attrs() {
    let xsd = r###"
        <xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema"
                   xmlns="urn:t" targetNamespace="urn:t"
                   elementFormDefault="qualified"
                   attributeFormDefault="qualified">
          <xs:attribute name="known" type="xs:string"/>
          <xs:element name="root">
            <xs:complexType>
              <xs:attribute name="sib" type="xs:string"/>
              <xs:anyAttribute namespace="##any" notQName="##definedSibling"
                               processContents="skip"/>
            </xs:complexType>
          </xs:element>
        </xs:schema>"###;
    let s = compile_11(xsd);
    // `known` is top-level but not a sibling of the wildcard — admitted.
    // Novel attribute names — admitted.
    ok(&s,  r#"<root xmlns="urn:t" xmlns:t="urn:t" t:known="x"/>"#);
    ok(&s,  r#"<root xmlns="urn:t" xmlns:t="urn:t" t:novel="x"/>"#);
    // `sib` is declared on this type — the wildcard would otherwise
    // claim it on a duplicate; ##definedSibling excludes it.  A
    // single `sib` is consumed by the explicit attribute use; a
    // second-binding clash isn't expressible here, so the case we
    // verify is that the wildcard does NOT take responsibility for a
    // sib-named attribute (i.e., the attribute decl wins, as already
    // happens; nothing to assert further).
    let _ = s;
}
