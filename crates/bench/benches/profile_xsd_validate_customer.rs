//! Tight inner loop for `samply` / `cargo flamegraph` on the
//! XSD validator's hot path.
//!
//! The `xsd` bench shows sup-xml at 1.36× libxml2 on the
//! customer1.xml fixture — the largest visible per-element gap in
//! the suite.  This driver compiles the schema once and then
//! validates the document N times in a tight loop, so a profiler
//! sees the validator's per-element machinery (content cursor,
//! namespace stack, attribute population, identity-constraint
//! tracking) as the dominant cost rather than schema compile or
//! XML parse.
//!
//! Run:
//!     cargo bench -p sup-xml-bench --bench profile_xsd_validate_customer
//!
//! Under samply:
//!     cargo build --release -p sup-xml-bench --bench profile_xsd_validate_customer
//!     samply record target/release/deps/profile_xsd_validate_customer-*

use std::time::Instant;

use sup_xml::xsd::Schema;

const CUSTOMER_XSD: &str = r#"<?xml version="1.0"?>
<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema">
  <xs:element name="table" type="TableType"/>
  <xs:complexType name="TableType">
    <xs:sequence>
      <xs:element name="T" type="RowType" minOccurs="0" maxOccurs="unbounded"/>
    </xs:sequence>
    <xs:attribute name="ID" type="xs:string" use="required"/>
  </xs:complexType>
  <xs:complexType name="RowType">
    <xs:sequence>
      <xs:element name="C_CUSTKEY"   type="xs:int"/>
      <xs:element name="C_NAME"      type="xs:string"/>
      <xs:element name="C_ADDRESS"   type="xs:string"/>
      <xs:element name="C_NATIONKEY" type="xs:int"/>
      <xs:element name="C_PHONE"     type="xs:string"/>
      <xs:element name="C_ACCTBAL"   type="xs:decimal"/>
      <xs:element name="C_MKTSEGMENT">
        <xs:simpleType>
          <xs:restriction base="xs:string">
            <xs:enumeration value="AUTOMOBILE"/>
            <xs:enumeration value="BUILDING"/>
            <xs:enumeration value="FURNITURE"/>
            <xs:enumeration value="HOUSEHOLD"/>
            <xs:enumeration value="MACHINERY"/>
          </xs:restriction>
        </xs:simpleType>
      </xs:element>
      <xs:element name="C_COMMENT"   type="xs:string"/>
    </xs:sequence>
  </xs:complexType>
</xs:schema>"#;

fn main() {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let xml_path = format!("{manifest}/../../tests/assets/xml/customer1.xml");
    let xml = std::fs::read_to_string(&xml_path)
        .unwrap_or_else(|e| panic!("read {xml_path}: {e}"));

    let schema = Schema::compile_str(CUSTOMER_XSD).expect("compile schema");

    // Sanity-check: one validation succeeds.  If a schema or
    // fixture drift breaks the comparison, fail loudly here rather
    // than silently profile an error path.
    schema.validate_str(&xml).expect("fixture must validate against the customer schema");

    // Default: enough iterations so a typical samply session
    // (~5–10 s) captures plenty of samples.  Override via
    // `SUPXML_PROFILE_ITERS` for shorter runs.
    let iters: u32 = std::env::var("SUPXML_PROFILE_ITERS")
        .ok().and_then(|s| s.parse().ok())
        .unwrap_or(200);

    eprintln!("profiling {iters} validations of customer1.xml ({} bytes)...",
        xml.len());

    let t0 = Instant::now();
    for _ in 0..iters {
        let r = schema.validate_str(&xml);
        std::hint::black_box(r).expect("validate");
    }
    let dt = t0.elapsed();

    let per_op_us = dt.as_secs_f64() / iters as f64 * 1e6;
    let mb_per_s  = (xml.len() as f64 * iters as f64)
        / dt.as_secs_f64() / (1024.0 * 1024.0);
    println!("\n{iters} iters × validate_str on customer1.xml");
    println!("  total: {:.2} s", dt.as_secs_f64());
    println!("  per-op: {per_op_us:.1} µs");
    println!("  throughput: {mb_per_s:.1} MB/s");
}
