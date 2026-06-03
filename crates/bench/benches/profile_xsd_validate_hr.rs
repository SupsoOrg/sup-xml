//! HR schema profile driver — the remaining 1.18× gap vs libxml2 in
//! the xsd bench.  Schema features: namespace-qualified
//! (elementFormDefault="qualified"), xs:date, xs:decimal with
//! facets, xs:NMTOKEN with a regex pattern, xs:string enumeration.
//!
//! Run:
//!     cargo build --profile=profiling -p sup-xml-bench --bench profile_xsd_validate_hr
//!     samply record target/profiling/deps/profile_xsd_validate_hr-*

use std::time::Instant;

use sup_xml::xsd::Schema;

const HR_XSD: &str = r#"<?xml version="1.0"?>
<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema"
           targetNamespace="urn:hr" xmlns="urn:hr"
           elementFormDefault="qualified">
  <xs:element name="employees" type="EmployeesType"/>
  <xs:complexType name="EmployeesType">
    <xs:sequence>
      <xs:element name="employee" type="EmployeeType" maxOccurs="unbounded"/>
    </xs:sequence>
  </xs:complexType>
  <xs:complexType name="EmployeeType">
    <xs:sequence>
      <xs:element name="firstName" type="xs:string"/>
      <xs:element name="lastName"  type="xs:string"/>
      <xs:element name="hireDate"  type="xs:date"/>
      <xs:element name="salary">
        <xs:simpleType>
          <xs:restriction base="xs:decimal">
            <xs:minInclusive value="0"/>
            <xs:fractionDigits value="2"/>
          </xs:restriction>
        </xs:simpleType>
      </xs:element>
      <xs:element name="status" type="StatusType"/>
    </xs:sequence>
    <xs:attribute name="empId" type="EmpId" use="required"/>
  </xs:complexType>
  <xs:simpleType name="EmpId">
    <xs:restriction base="xs:NMTOKEN">
      <xs:pattern value="E\d{6}"/>
    </xs:restriction>
  </xs:simpleType>
  <xs:simpleType name="StatusType">
    <xs:restriction base="xs:string">
      <xs:enumeration value="active"/>
      <xs:enumeration value="leave"/>
      <xs:enumeration value="terminated"/>
    </xs:restriction>
  </xs:simpleType>
</xs:schema>"#;

fn build_hr_xml() -> String {
    let mut xml = String::from(r#"<?xml version="1.0"?><employees xmlns="urn:hr">"#);
    for i in 0..200 {
        xml.push_str(&format!(
            "<employee empId=\"E{:06}\">\
             <firstName>First{i}</firstName>\
             <lastName>Last{i}</lastName>\
             <hireDate>20{:02}-{:02}-{:02}</hireDate>\
             <salary>{}.{:02}</salary>\
             <status>{}</status></employee>",
             i, i % 25, (i % 12) + 1, (i % 28) + 1,
             50000 + i * 7, i % 100,
             ["active", "leave", "terminated"][i % 3]));
    }
    xml.push_str("</employees>");
    xml
}

fn main() {
    let xml = build_hr_xml();
    let schema = Schema::compile_str(HR_XSD).expect("compile");
    schema.validate_str(&xml).expect("fixture must validate");

    let iters: u32 = std::env::var("SUPXML_PROFILE_ITERS")
        .ok().and_then(|s| s.parse().ok()).unwrap_or(5000);

    eprintln!("profiling {iters} validations of HR ({} bytes)…", xml.len());
    let t0 = Instant::now();
    for _ in 0..iters {
        std::hint::black_box(schema.validate_str(&xml)).expect("validate");
    }
    let dt = t0.elapsed();
    println!("\n{iters} iters × validate_str on HR");
    println!("  total: {:.2} s", dt.as_secs_f64());
    println!("  per-op: {:.1} µs", dt.as_secs_f64() / iters as f64 * 1e6);
    println!("  throughput: {:.1} MB/s",
        (xml.len() as f64 * iters as f64) / dt.as_secs_f64() / (1024.0 * 1024.0));
}
