#![no_main]

//! Fuzz target — given a fixed valid schema, feed arbitrary bytes
//! (interpreted as UTF-8) to [`Schema::validate_str`] and assert no
//! panic.  Validation may legitimately produce errors; we're hunting
//! parser/validator crashes.
//!
//! Three reasonably-different schema shapes (purchase order, key+keyref,
//! choice/all) are mixed into the corpus by deriving the schema from
//! the first input byte — fuzzer can drive validation against each
//! schema by varying its first byte.  Beyond that, the rest of the
//! input feeds straight into validate_str.

use libfuzzer_sys::fuzz_target;
use sup_xml_core::xsd::Schema;
use std::sync::OnceLock;

const PO_XSD: &str = r#"<?xml version="1.0"?>
<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema"
           targetNamespace="urn:po" xmlns="urn:po"
           elementFormDefault="qualified">
  <xs:element name="purchaseOrder" type="POType"/>
  <xs:complexType name="POType">
    <xs:sequence>
      <xs:element name="lineItem" type="LineItem" maxOccurs="unbounded"/>
    </xs:sequence>
    <xs:attribute name="orderDate" type="xs:date" use="required"/>
  </xs:complexType>
  <xs:complexType name="LineItem">
    <xs:sequence>
      <xs:element name="part"     type="xs:string"/>
      <xs:element name="quantity" type="xs:positiveInteger"/>
      <xs:element name="price"    type="xs:decimal"/>
    </xs:sequence>
  </xs:complexType>
</xs:schema>"#;

const KEY_XSD: &str = r#"<?xml version="1.0"?>
<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema"
           targetNamespace="urn:k" xmlns="urn:k"
           elementFormDefault="qualified">
  <xs:element name="catalog">
    <xs:complexType>
      <xs:sequence>
        <xs:element name="part" maxOccurs="unbounded">
          <xs:complexType>
            <xs:attribute name="num" type="xs:string" use="required"/>
          </xs:complexType>
        </xs:element>
      </xs:sequence>
    </xs:complexType>
    <xs:key name="partKey">
      <xs:selector xpath=".//part"/>
      <xs:field xpath="@num"/>
    </xs:key>
  </xs:element>
</xs:schema>"#;

const CHOICE_XSD: &str = r#"<?xml version="1.0"?>
<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema"
           targetNamespace="urn:c" xmlns="urn:c"
           elementFormDefault="qualified">
  <xs:element name="thing">
    <xs:complexType>
      <xs:choice>
        <xs:element name="left"  type="xs:int"/>
        <xs:element name="right" type="xs:string"/>
      </xs:choice>
    </xs:complexType>
  </xs:element>
</xs:schema>"#;

static SCHEMAS: OnceLock<[Schema; 3]> = OnceLock::new();

fn schemas() -> &'static [Schema; 3] {
    SCHEMAS.get_or_init(|| [
        Schema::compile_str(PO_XSD).expect("PO_XSD compiles"),
        Schema::compile_str(KEY_XSD).expect("KEY_XSD compiles"),
        Schema::compile_str(CHOICE_XSD).expect("CHOICE_XSD compiles"),
    ])
}

fuzz_target!(|data: &[u8]| {
    if data.is_empty() { return; }
    let schemas = schemas();
    let schema = &schemas[(data[0] as usize) % schemas.len()];
    if let Ok(xml) = std::str::from_utf8(&data[1..]) {
        let _ = schema.validate_str(xml);
    }
});
