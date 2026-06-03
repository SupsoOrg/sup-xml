//! Compile-time breakdown for the HR-style schema.
//!
//! The `xsd` bench reports HR compile at ~5–6× libxml2.  This probe
//! strips one feature at a time so the dominant cost is visible.
//! Run:
//!     cargo bench -p sup-xml-bench --bench profile_xsd_compile

#![allow(clippy::missing_safety_doc)]

use std::os::raw::{c_char, c_int, c_void};
use std::time::Instant;

use sup_xml::xsd::Schema;

type XmlSchemaParserCtxtPtr = *mut c_void;
type XmlSchemaPtr            = *mut c_void;

unsafe extern "C" {
    fn xmlSchemaNewMemParserCtxt(buffer: *const c_char, size: c_int) -> XmlSchemaParserCtxtPtr;
    fn xmlSchemaParse(ctxt: XmlSchemaParserCtxtPtr) -> XmlSchemaPtr;
    fn xmlSchemaFreeParserCtxt(ctxt: XmlSchemaParserCtxtPtr);
    fn xmlSchemaFree(schema: XmlSchemaPtr);
    fn xmlSchemaSetParserErrors(
        ctxt: XmlSchemaParserCtxtPtr,
        err: Option<unsafe extern "C" fn()>,
        warn: Option<unsafe extern "C" fn()>,
        ctx: *mut c_void,
    );
    fn xmlSetGenericErrorFunc(ctx: *mut c_void, handler: Option<unsafe extern "C" fn()>);
}

unsafe extern "C" fn libxml2_swallow() {}

unsafe fn libxml2_silent_parse(xsd: &[u8]) -> XmlSchemaPtr {
    unsafe {
        let ctx = xmlSchemaNewMemParserCtxt(xsd.as_ptr() as *const c_char, xsd.len() as c_int);
        if ctx.is_null() { return std::ptr::null_mut(); }
        xmlSchemaSetParserErrors(ctx, None, None, std::ptr::null_mut());
        let s = xmlSchemaParse(ctx);
        xmlSchemaFreeParserCtxt(ctx);
        s
    }
}

fn min_ns<F: FnMut()>(n: usize, mut f: F) -> u128 {
    let mut best = u128::MAX;
    for _ in 0..n {
        let t = Instant::now();
        f();
        let elapsed = t.elapsed().as_nanos();
        if elapsed < best { best = elapsed; }
    }
    best
}

fn fmt_ns(ns: u128) -> String {
    if ns >= 1_000_000      { format!("{:.2} ms", ns as f64 / 1e6) }
    else if ns >= 1_000     { format!("{:.2} µs", ns as f64 / 1e3) }
    else                    { format!("{ns} ns") }
}

fn time_compile(label: &str, xsd: &str, n: usize) {
    let sx = min_ns(n, || {
        let s = Schema::compile_str(xsd).expect("sup-xml compile");
        std::hint::black_box(s);
    });
    let lx = min_ns(n, || unsafe {
        let s = libxml2_silent_parse(xsd.as_bytes());
        assert!(!s.is_null(), "libxml2 compile failed: {label}");
        xmlSchemaFree(s);
    });
    let ratio = sx as f64 / lx as f64;
    let r = if ratio >= 1.0 { format!("{ratio:.2}× slower") }
            else            { format!("{:.2}× faster", 1.0 / ratio) };
    println!("    {label:<46}  sup-xml: {:>10}    libxml2: {:>10}    {r}",
        fmt_ns(sx), fmt_ns(lx));
}

// ── HR schema, stripped down one feature at a time ──────────────────────────

/// Full schema (matches the one in the main xsd bench).
const HR_FULL: &str = r#"<?xml version="1.0"?>
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

/// Drop the `<xs:pattern>` facet.  EmpId becomes a bare NMTOKEN.
const HR_NO_PATTERN: &str = r#"<?xml version="1.0"?>
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
    <xs:attribute name="empId" type="xs:NMTOKEN" use="required"/>
  </xs:complexType>
  <xs:simpleType name="StatusType">
    <xs:restriction base="xs:string">
      <xs:enumeration value="active"/>
      <xs:enumeration value="leave"/>
      <xs:enumeration value="terminated"/>
    </xs:restriction>
  </xs:simpleType>
</xs:schema>"#;

/// Drop the StatusType enumeration; status becomes a plain string.
const HR_NO_ENUM: &str = r#"<?xml version="1.0"?>
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
      <xs:element name="status" type="xs:string"/>
    </xs:sequence>
    <xs:attribute name="empId" type="EmpId" use="required"/>
  </xs:complexType>
  <xs:simpleType name="EmpId">
    <xs:restriction base="xs:NMTOKEN">
      <xs:pattern value="E\d{6}"/>
    </xs:restriction>
  </xs:simpleType>
</xs:schema>"#;

/// Drop the salary decimal facets; salary becomes a plain decimal.
const HR_NO_DECIMAL_FACETS: &str = r#"<?xml version="1.0"?>
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
      <xs:element name="salary"    type="xs:decimal"/>
      <xs:element name="status"    type="StatusType"/>
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

/// All facets dropped — minimal skeleton, every field is a built-in.
const HR_SKELETON: &str = r#"<?xml version="1.0"?>
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
      <xs:element name="salary"    type="xs:decimal"/>
      <xs:element name="status"    type="xs:string"/>
    </xs:sequence>
    <xs:attribute name="empId" type="xs:NMTOKEN" use="required"/>
  </xs:complexType>
</xs:schema>"#;

/// Skeleton + just the pattern facet, to isolate pattern compile cost
/// against a near-zero baseline.
const HR_PATTERN_ONLY: &str = r#"<?xml version="1.0"?>
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
      <xs:element name="salary"    type="xs:decimal"/>
      <xs:element name="status"    type="xs:string"/>
    </xs:sequence>
    <xs:attribute name="empId" type="EmpId" use="required"/>
  </xs:complexType>
  <xs:simpleType name="EmpId">
    <xs:restriction base="xs:NMTOKEN">
      <xs:pattern value="E\d{6}"/>
    </xs:restriction>
  </xs:simpleType>
</xs:schema>"#;

/// Same as PATTERN_ONLY but with the pattern simplified to a trivial
/// single-char regex.  Helps separate "having any pattern at all"
/// overhead from "compiling this specific regex".
const HR_PATTERN_TRIVIAL: &str = r#"<?xml version="1.0"?>
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
      <xs:element name="salary"    type="xs:decimal"/>
      <xs:element name="status"    type="xs:string"/>
    </xs:sequence>
    <xs:attribute name="empId" type="EmpId" use="required"/>
  </xs:complexType>
  <xs:simpleType name="EmpId">
    <xs:restriction base="xs:NMTOKEN">
      <xs:pattern value="."/>
    </xs:restriction>
  </xs:simpleType>
</xs:schema>"#;

fn main() {
    unsafe { xmlSetGenericErrorFunc(std::ptr::null_mut(), Some(libxml2_swallow)); }
    let n = 100;

    println!("\nHR-schema compile breakdown (min over {n} iters)\n");
    time_compile("HR (full)",                 HR_FULL,              n);
    time_compile("HR (no pattern)",           HR_NO_PATTERN,        n);
    time_compile("HR (no enumeration)",       HR_NO_ENUM,           n);
    time_compile("HR (no decimal facets)",    HR_NO_DECIMAL_FACETS, n);
    time_compile("HR (skeleton — no facets)", HR_SKELETON,          n);
    println!();
    time_compile("HR (skeleton + E\\d{6} pattern)", HR_PATTERN_ONLY,    n);
    time_compile("HR (skeleton + trivial '.' pattern)", HR_PATTERN_TRIVIAL, n);
    println!();
}
