//! XSD validator benchmark — sup_xml::xsd vs libxml2.
//!
//! Three phases reported per fixture:
//!
//! * **compile**: parse the XSD source into a ready-to-validate Schema.
//!   Done once per integration in real usage; we measure it because
//!   schema-rich workloads (SOAP, XBRL, OOXML) re-compile on cold start.
//! * **validate**: run a *conforming* instance through the compiled
//!   schema and accept it.  This is the hot path most users see.
//! * **reject**: run a *non-conforming* mutation of the same instance
//!   through the schema and report the error.  Captures the cost of
//!   producing a diagnostic, not just the cost of saying "no".
//!
//! Fixtures are grouped into four categories:
//!
//! * **real-world** — schemas modelled on actual formats (Atom, POM, RSS,
//!   sitemap, customer table, server config, employee directory, event
//!   log) paired with conforming instances and one mutated instance per
//!   schema.
//! * **scaling** — same schema (TPC-H customer table), instance sizes
//!   varying across two orders of magnitude.  Shows how throughput
//!   tracks document size.
//! * **synthetic** — single-feature stress fixtures (wide attribute load,
//!   deep nesting, enumeration matching, pattern matching) to isolate
//!   where each implementation spends time.
//! * **fixture pair** — the on-disk `xml_xsd_1.xml` + `xml_xsd_2.xml`
//!   pair: a real B2B order with deeply-nested inline complex types.
//!
//! Each category emits a header row, a table with one line per fixture,
//! and a geomean summary giving sup_xml's median speedup vs libxml2
//! across that category for compile, validate, and reject.  A final
//! overall summary aggregates across all categories.
//!
//! Run:
//!     cargo bench -p sup-xml-bench --bench xsd
//!     SUPXML_XSD_ITERS=200 cargo bench -p sup-xml-bench --bench xsd
//!
//! Output is a plain text table.  No criterion machinery — same fast-
//! dev-loop philosophy as `mini.rs`.

#![allow(clippy::missing_safety_doc)]

use std::os::raw::{c_char, c_int, c_void};
use std::time::Instant;

use sup_xml::xsd::Schema;

// ── libxml2 schema-validation FFI ────────────────────────────────────────────

type XmlSchemaParserCtxtPtr = *mut c_void;
type XmlSchemaPtr            = *mut c_void;
type XmlSchemaValidCtxtPtr   = *mut c_void;
type XmlDocPtr               = *mut c_void;

unsafe extern "C" {
    fn xmlReadMemory(
        buffer: *const c_char,
        size: c_int,
        url: *const c_char,
        encoding: *const c_char,
        options: c_int,
    ) -> XmlDocPtr;
    fn xmlFreeDoc(doc: XmlDocPtr);

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

    fn xmlSchemaNewValidCtxt(schema: XmlSchemaPtr) -> XmlSchemaValidCtxtPtr;
    fn xmlSchemaFreeValidCtxt(ctxt: XmlSchemaValidCtxtPtr);
    fn xmlSchemaValidateDoc(ctxt: XmlSchemaValidCtxtPtr, doc: XmlDocPtr) -> c_int;
    fn xmlSchemaSetValidErrors(
        ctxt: XmlSchemaValidCtxtPtr,
        err: Option<unsafe extern "C" fn()>,
        warn: Option<unsafe extern "C" fn()>,
        ctx: *mut c_void,
    );

    fn xmlSetGenericErrorFunc(ctx: *mut c_void, handler: Option<unsafe extern "C" fn()>);
}

/// libxml2's *generic* error handler is what its schema validator falls
/// back to when no per-context handler is set.  The C signature is
/// variadic (`void(*)(void*, const char*, ...)`); ignoring the trailing
/// args here is sound on every ABI we target because libxml2 is the
/// caller and uses caller-cleanup.
unsafe extern "C" fn libxml2_swallow() {}

fn install_libxml2_silencer() {
    unsafe { xmlSetGenericErrorFunc(std::ptr::null_mut(), Some(libxml2_swallow)); }
}

/// libxml2 will write to stderr by default; silence it so the bench
/// output stays tidy when a fixture deliberately triggers a diagnostic.
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

/// Returns `Some(true)` if the document validates, `Some(false)` if it
/// is rejected, and `None` if libxml2 could not even parse it.
unsafe fn libxml2_validate(schema: XmlSchemaPtr, xml: &[u8]) -> Option<bool> {
    unsafe {
        let doc = xmlReadMemory(
            xml.as_ptr() as *const c_char,
            xml.len() as c_int,
            std::ptr::null(),
            std::ptr::null(),
            0,
        );
        if doc.is_null() { return None; }
        let v = xmlSchemaNewValidCtxt(schema);
        xmlSchemaSetValidErrors(v, None, None, std::ptr::null_mut());
        let ok = xmlSchemaValidateDoc(v, doc) == 0;
        xmlSchemaFreeValidCtxt(v);
        xmlFreeDoc(doc);
        Some(ok)
    }
}

// ── fixture model ────────────────────────────────────────────────────────────

#[derive(Copy, Clone, PartialEq, Eq)]
enum Category { RealWorld, Scaling, Synthetic, FixturePair }

impl Category {
    fn header(self) -> &'static str {
        match self {
            Self::RealWorld   => "real-world schemas",
            Self::Scaling     => "scaling — TPC-H customer table",
            Self::Synthetic   => "synthetic stress fixtures",
            Self::FixturePair => "on-disk xml/xsd fixture pair",
        }
    }
}

struct Fixture {
    label:        &'static str,
    category:     Category,
    xsd:          String,
    /// A conforming instance.
    valid_xml:    String,
    /// A non-conforming mutation of `valid_xml` used to measure
    /// rejection cost.  `None` means the bench skips the reject column
    /// for this fixture (e.g. when no clean single-character mutation
    /// exists).
    invalid_xml:  Option<String>,
}

// ── real-world schemas (lifted from crates/api/tests/xsd_real_world.rs) ──────

fn purchase_order() -> Fixture {
    let xsd = r#"<?xml version="1.0"?>
<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema"
           targetNamespace="urn:po" xmlns="urn:po"
           elementFormDefault="qualified">
  <xs:element name="purchaseOrder" type="PurchaseOrderType"/>
  <xs:complexType name="PurchaseOrderType">
    <xs:sequence>
      <xs:element name="shipTo"  type="USAddress"/>
      <xs:element name="billTo"  type="USAddress"/>
      <xs:element name="comment" type="xs:string" minOccurs="0"/>
      <xs:element name="items"   type="Items"/>
    </xs:sequence>
    <xs:attribute name="orderDate" type="xs:date" use="required"/>
  </xs:complexType>
  <xs:complexType name="USAddress">
    <xs:sequence>
      <xs:element name="name"   type="xs:string"/>
      <xs:element name="street" type="xs:string"/>
      <xs:element name="city"   type="xs:string"/>
      <xs:element name="state"  type="USState"/>
      <xs:element name="zip"    type="xs:string"/>
    </xs:sequence>
    <xs:attribute name="country" type="xs:string" use="required" fixed="US"/>
  </xs:complexType>
  <xs:simpleType name="USState">
    <xs:restriction base="xs:string">
      <xs:enumeration value="AK"/><xs:enumeration value="AL"/>
      <xs:enumeration value="CA"/><xs:enumeration value="MA"/>
      <xs:enumeration value="NY"/><xs:enumeration value="TX"/>
      <xs:enumeration value="WA"/>
    </xs:restriction>
  </xs:simpleType>
  <xs:complexType name="Items">
    <xs:sequence>
      <xs:element name="item" type="ItemType" minOccurs="1" maxOccurs="unbounded"/>
    </xs:sequence>
  </xs:complexType>
  <xs:complexType name="ItemType">
    <xs:sequence>
      <xs:element name="productName" type="xs:string"/>
      <xs:element name="quantity">
        <xs:simpleType>
          <xs:restriction base="xs:positiveInteger">
            <xs:maxExclusive value="100"/>
          </xs:restriction>
        </xs:simpleType>
      </xs:element>
      <xs:element name="USPrice">
        <xs:simpleType>
          <xs:restriction base="xs:decimal">
            <xs:totalDigits value="9"/>
            <xs:fractionDigits value="2"/>
          </xs:restriction>
        </xs:simpleType>
      </xs:element>
      <xs:element name="comment" type="xs:string" minOccurs="0"/>
    </xs:sequence>
    <xs:attribute name="partNum" type="xs:string" use="required"/>
  </xs:complexType>
</xs:schema>"#;
    let valid = r#"<?xml version="1.0"?>
<purchaseOrder xmlns="urn:po" orderDate="2024-03-15">
  <shipTo country="US"><name>Alice</name><street>123 Main</street>
    <city>Boston</city><state>MA</state><zip>02101</zip></shipTo>
  <billTo country="US"><name>Bob</name><street>456 Elm</street>
    <city>Seattle</city><state>WA</state><zip>98101</zip></billTo>
  <items>
    <item partNum="A1"><productName>Widget</productName>
      <quantity>3</quantity><USPrice>9.99</USPrice></item>
    <item partNum="A2"><productName>Gadget</productName>
      <quantity>7</quantity><USPrice>19.99</USPrice></item>
  </items>
</purchaseOrder>"#;
    let invalid = valid.replace("<state>WA</state>", "<state>ZZ</state>");
    Fixture {
        label: "purchaseOrder (W3C primer)",
        category: Category::RealWorld,
        xsd: xsd.into(),
        valid_xml: valid.into(),
        invalid_xml: Some(invalid),
    }
}

fn atom_feed() -> Fixture {
    let xsd = r#"<?xml version="1.0"?>
<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema"
           targetNamespace="http://www.w3.org/2005/Atom"
           xmlns="http://www.w3.org/2005/Atom"
           elementFormDefault="qualified">
  <xs:element name="feed" type="FeedType"/>
  <xs:complexType name="FeedType">
    <xs:sequence>
      <xs:element name="title"   type="TextType"/>
      <xs:element name="id"      type="xs:anyURI"/>
      <xs:element name="updated" type="xs:dateTime"/>
      <xs:element name="link"    type="LinkType" minOccurs="0" maxOccurs="unbounded"/>
      <xs:element name="author"  type="PersonType" minOccurs="0" maxOccurs="unbounded"/>
      <xs:element name="entry"   type="EntryType" minOccurs="0" maxOccurs="unbounded"/>
    </xs:sequence>
  </xs:complexType>
  <xs:complexType name="EntryType">
    <xs:sequence>
      <xs:element name="title"     type="TextType"/>
      <xs:element name="id"        type="xs:anyURI"/>
      <xs:element name="updated"   type="xs:dateTime"/>
      <xs:element name="published" type="xs:dateTime" minOccurs="0"/>
      <xs:element name="summary"   type="TextType" minOccurs="0"/>
      <xs:element name="link"      type="LinkType" minOccurs="0" maxOccurs="unbounded"/>
      <xs:element name="author"    type="PersonType" minOccurs="0" maxOccurs="unbounded"/>
    </xs:sequence>
  </xs:complexType>
  <xs:complexType name="TextType">
    <xs:simpleContent>
      <xs:extension base="xs:string">
        <xs:attribute name="type" type="TextKind" default="text"/>
      </xs:extension>
    </xs:simpleContent>
  </xs:complexType>
  <xs:simpleType name="TextKind">
    <xs:restriction base="xs:string">
      <xs:enumeration value="text"/>
      <xs:enumeration value="html"/>
      <xs:enumeration value="xhtml"/>
    </xs:restriction>
  </xs:simpleType>
  <xs:complexType name="LinkType">
    <xs:attribute name="href"     type="xs:anyURI" use="required"/>
    <xs:attribute name="rel"      type="xs:string"/>
    <xs:attribute name="type"     type="xs:string"/>
    <xs:attribute name="hreflang" type="xs:language"/>
    <xs:attribute name="title"    type="xs:string"/>
  </xs:complexType>
  <xs:complexType name="PersonType">
    <xs:sequence>
      <xs:element name="name"  type="xs:string"/>
      <xs:element name="uri"   type="xs:anyURI" minOccurs="0"/>
      <xs:element name="email" type="xs:string" minOccurs="0"/>
    </xs:sequence>
  </xs:complexType>
</xs:schema>"#;
    // 40 entries — enough to amortise per-element validation cost.
    let mut xml = String::from(
        r#"<?xml version="1.0"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <title>Example Feed</title>
  <id>https://example.org/feed</id>
  <updated>2024-03-15T10:00:00Z</updated>
  <link href="https://example.org/" rel="alternate"/>
  <author><name>Alice</name><uri>https://example.org/~alice</uri></author>
"#);
    for i in 0..40 {
        xml.push_str(&format!(
            r#"  <entry>
    <title type="html">Post &amp; entry {i}</title>
    <id>https://example.org/posts/{i}</id>
    <updated>2024-03-15T10:{:02}:00Z</updated>
    <summary>Summary number {i}.</summary>
  </entry>
"#, i % 60));
    }
    xml.push_str("</feed>");
    let invalid = xml.replacen(r#"type="html""#, r#"type="markdown""#, 1);
    Fixture {
        label: "atom feed (40 entries)",
        category: Category::RealWorld,
        xsd: xsd.into(),
        valid_xml: xml,
        invalid_xml: Some(invalid),
    }
}

fn pom() -> Fixture {
    let xsd = r#"<?xml version="1.0"?>
<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema"
           targetNamespace="urn:pom" xmlns="urn:pom"
           elementFormDefault="qualified">
  <xs:element name="project" type="ProjectType"/>
  <xs:complexType name="ProjectType">
    <xs:sequence>
      <xs:element name="modelVersion" type="xs:string" fixed="4.0.0"/>
      <xs:element name="groupId"      type="GroupOrArtifactId"/>
      <xs:element name="artifactId"   type="GroupOrArtifactId"/>
      <xs:element name="version"      type="VersionString"/>
      <xs:element name="packaging">
        <xs:simpleType>
          <xs:restriction base="xs:string">
            <xs:enumeration value="jar"/>
            <xs:enumeration value="war"/>
            <xs:enumeration value="pom"/>
          </xs:restriction>
        </xs:simpleType>
      </xs:element>
      <xs:element name="dependencies" type="DependenciesType" minOccurs="0"/>
    </xs:sequence>
  </xs:complexType>
  <xs:simpleType name="GroupOrArtifactId">
    <xs:restriction base="xs:string">
      <xs:pattern value="[a-z][a-z0-9_\-\.]*"/>
      <xs:minLength value="1"/>
      <xs:maxLength value="100"/>
    </xs:restriction>
  </xs:simpleType>
  <xs:simpleType name="VersionString">
    <xs:restriction base="xs:string">
      <xs:pattern value="\d+\.\d+\.\d+(-[A-Za-z0-9\-\.]+)?"/>
    </xs:restriction>
  </xs:simpleType>
  <xs:complexType name="DependenciesType">
    <xs:sequence>
      <xs:element name="dependency" type="DependencyType" maxOccurs="unbounded"/>
    </xs:sequence>
  </xs:complexType>
  <xs:complexType name="DependencyType">
    <xs:sequence>
      <xs:element name="groupId"    type="GroupOrArtifactId"/>
      <xs:element name="artifactId" type="GroupOrArtifactId"/>
      <xs:element name="version"    type="VersionString"/>
      <xs:element name="scope" minOccurs="0">
        <xs:simpleType>
          <xs:restriction base="xs:string">
            <xs:enumeration value="compile"/>
            <xs:enumeration value="provided"/>
            <xs:enumeration value="runtime"/>
            <xs:enumeration value="test"/>
          </xs:restriction>
        </xs:simpleType>
      </xs:element>
    </xs:sequence>
  </xs:complexType>
</xs:schema>"#;
    let mut xml = String::from(
        r#"<?xml version="1.0"?>
<project xmlns="urn:pom">
  <modelVersion>4.0.0</modelVersion>
  <groupId>org.example</groupId>
  <artifactId>my-app</artifactId>
  <version>1.2.3</version>
  <packaging>jar</packaging>
  <dependencies>
"#);
    for i in 0..50 {
        xml.push_str(&format!(
            "    <dependency><groupId>com.lib{i}</groupId>\
             <artifactId>lib-core-{i}</artifactId>\
             <version>{}.{}.{}</version>\
             <scope>compile</scope></dependency>\n",
             i % 10, i, i % 100));
    }
    xml.push_str("  </dependencies>\n</project>");
    // Mutate a single groupId to use uppercase, violating the pattern.
    let invalid = xml.replacen("<groupId>com.lib0</groupId>",
                               "<groupId>Com.Lib0</groupId>", 1);
    Fixture {
        label: "pom (50 deps, pattern-heavy)",
        category: Category::RealWorld,
        xsd: xsd.into(),
        valid_xml: xml,
        invalid_xml: Some(invalid),
    }
}

fn hr() -> Fixture {
    let xsd = r#"<?xml version="1.0"?>
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
    let mut xml = String::from(
        r#"<?xml version="1.0"?><employees xmlns="urn:hr">"#);
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
    let invalid = xml.replacen(r#"<status>active</status>"#,
                               r#"<status>vacationing</status>"#, 1);
    Fixture {
        label: "hr (200 employees)",
        category: Category::RealWorld,
        xsd: xsd.into(),
        valid_xml: xml,
        invalid_xml: Some(invalid),
    }
}

fn config() -> Fixture {
    let xsd = r#"<?xml version="1.0"?>
<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema"
           targetNamespace="urn:cfg" xmlns="urn:cfg"
           elementFormDefault="qualified">
  <xs:element name="config" type="ConfigType"/>
  <xs:complexType name="ConfigType">
    <xs:sequence>
      <xs:element name="server"   type="ServerType"/>
      <xs:element name="logging"  type="LoggingType"/>
      <xs:element name="features" type="FeaturesType" minOccurs="0"/>
    </xs:sequence>
  </xs:complexType>
  <xs:complexType name="ServerType">
    <xs:sequence>
      <xs:element name="host" type="xs:string"/>
      <xs:element name="port">
        <xs:simpleType>
          <xs:restriction base="xs:unsignedShort">
            <xs:minInclusive value="1"/>
            <xs:maxInclusive value="65535"/>
          </xs:restriction>
        </xs:simpleType>
      </xs:element>
      <xs:element name="tls" type="xs:boolean" default="false"/>
    </xs:sequence>
  </xs:complexType>
  <xs:complexType name="LoggingType">
    <xs:sequence>
      <xs:element name="level">
        <xs:simpleType>
          <xs:restriction base="xs:string">
            <xs:enumeration value="trace"/>
            <xs:enumeration value="debug"/>
            <xs:enumeration value="info"/>
            <xs:enumeration value="warn"/>
            <xs:enumeration value="error"/>
          </xs:restriction>
        </xs:simpleType>
      </xs:element>
      <xs:choice>
        <xs:element name="file" type="xs:string"/>
        <xs:element name="syslog">
          <xs:complexType>
            <xs:attribute name="facility" type="xs:string" default="local0"/>
          </xs:complexType>
        </xs:element>
        <xs:element name="stdout"/>
      </xs:choice>
    </xs:sequence>
  </xs:complexType>
  <xs:complexType name="FeaturesType">
    <xs:sequence>
      <xs:element name="metrics"   type="xs:boolean" minOccurs="0"/>
      <xs:element name="profiling" type="xs:boolean" minOccurs="0"/>
    </xs:sequence>
  </xs:complexType>
</xs:schema>"#;
    let valid = r#"<?xml version="1.0"?>
<config xmlns="urn:cfg">
  <server><host>0.0.0.0</host><port>8443</port><tls>true</tls></server>
  <logging><level>info</level><file>/var/log/app.log</file></logging>
  <features><metrics>true</metrics><profiling>false</profiling></features>
</config>"#;
    let invalid = valid.replace("<port>8443</port>", "<port>99999</port>");
    Fixture {
        label: "config (choice + range)",
        category: Category::RealWorld,
        xsd: xsd.into(),
        valid_xml: valid.into(),
        invalid_xml: Some(invalid),
    }
}

fn event_log() -> Fixture {
    let xsd = r#"<?xml version="1.0"?>
<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema"
           targetNamespace="urn:eventlog" xmlns="urn:eventlog"
           elementFormDefault="qualified">
  <xs:complexType name="Event" abstract="true">
    <xs:sequence>
      <xs:element name="timestamp" type="xs:dateTime"/>
    </xs:sequence>
    <xs:attribute name="id"     type="xs:string" use="required"/>
    <xs:attribute name="source" type="xs:string"/>
  </xs:complexType>
  <xs:complexType name="LoginEvent">
    <xs:complexContent>
      <xs:extension base="Event">
        <xs:sequence>
          <xs:element name="user" type="xs:string"/>
          <xs:element name="ip"   type="xs:string"/>
        </xs:sequence>
      </xs:extension>
    </xs:complexContent>
  </xs:complexType>
  <xs:complexType name="LogoutEvent">
    <xs:complexContent>
      <xs:extension base="Event">
        <xs:sequence>
          <xs:element name="user" type="xs:string"/>
        </xs:sequence>
      </xs:extension>
    </xs:complexContent>
  </xs:complexType>
  <xs:complexType name="ErrorEvent">
    <xs:complexContent>
      <xs:extension base="Event">
        <xs:sequence>
          <xs:element name="code"    type="xs:int"/>
          <xs:element name="message" type="xs:string"/>
        </xs:sequence>
      </xs:extension>
    </xs:complexContent>
  </xs:complexType>
  <xs:element name="event"  type="Event" abstract="true"/>
  <xs:element name="login"  type="LoginEvent"  substitutionGroup="event"/>
  <xs:element name="logout" type="LogoutEvent" substitutionGroup="event"/>
  <xs:element name="error"  type="ErrorEvent"  substitutionGroup="event"/>
  <xs:element name="log">
    <xs:complexType>
      <xs:sequence>
        <xs:element ref="event" maxOccurs="unbounded"/>
      </xs:sequence>
    </xs:complexType>
  </xs:element>
</xs:schema>"#;
    let mut xml = String::from(r#"<log xmlns="urn:eventlog">"#);
    for i in 0..100 {
        let kind = i % 3;
        let stamp = format!("2026-05-16T{:02}:{:02}:00Z", i / 60 % 24, i % 60);
        match kind {
            0 => xml.push_str(&format!(
                "<login id=\"e{i}\" source=\"web\">\
                 <timestamp>{stamp}</timestamp>\
                 <user>u{i}</user><ip>10.0.0.{}</ip></login>", i % 255)),
            1 => xml.push_str(&format!(
                "<logout id=\"e{i}\"><timestamp>{stamp}</timestamp>\
                 <user>u{i}</user></logout>")),
            _ => xml.push_str(&format!(
                "<error id=\"e{i}\" source=\"api\">\
                 <timestamp>{stamp}</timestamp>\
                 <code>{}</code><message>err {i}</message></error>",
                 500 + i % 50)),
        }
    }
    xml.push_str("</log>");
    let invalid = xml.replacen("<code>500</code>", "<code>not-a-number</code>", 1);
    Fixture {
        label: "event log (substitution groups, 100 events)",
        category: Category::RealWorld,
        xsd: xsd.into(),
        valid_xml: xml,
        invalid_xml: Some(invalid),
    }
}

fn sitemap_real() -> Option<Fixture> {
    let xsd = r#"<?xml version="1.0"?>
<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema"
           targetNamespace="http://www.sitemaps.org/schemas/sitemap/0.9"
           xmlns="http://www.sitemaps.org/schemas/sitemap/0.9"
           elementFormDefault="qualified">
  <xs:element name="urlset" type="UrlSetType"/>
  <xs:complexType name="UrlSetType">
    <xs:sequence>
      <xs:element name="url" type="UrlType" minOccurs="0" maxOccurs="unbounded"/>
    </xs:sequence>
  </xs:complexType>
  <xs:complexType name="UrlType">
    <xs:sequence>
      <xs:element name="loc"        type="xs:anyURI"/>
      <xs:element name="lastmod"    type="xs:string" minOccurs="0"/>
      <xs:element name="changefreq" minOccurs="0">
        <xs:simpleType>
          <xs:restriction base="xs:string">
            <xs:enumeration value="always"/><xs:enumeration value="hourly"/>
            <xs:enumeration value="daily"/><xs:enumeration value="weekly"/>
            <xs:enumeration value="monthly"/><xs:enumeration value="yearly"/>
            <xs:enumeration value="never"/>
          </xs:restriction>
        </xs:simpleType>
      </xs:element>
      <xs:element name="priority" minOccurs="0">
        <xs:simpleType>
          <xs:restriction base="xs:decimal">
            <xs:minInclusive value="0.0"/>
            <xs:maxInclusive value="1.0"/>
          </xs:restriction>
        </xs:simpleType>
      </xs:element>
    </xs:sequence>
  </xs:complexType>
</xs:schema>"#;
    let manifest = env!("CARGO_MANIFEST_DIR");
    let xml = std::fs::read_to_string(format!("{manifest}/../../tests/assets/xml/sitemap.xml")).ok()?;
    let invalid = xml.replacen("<changefreq>", "<changefreq>BOGUS-PREFIX-", 1);
    let invalid = if invalid == xml { None } else { Some(invalid) };
    Some(Fixture {
        label: "sitemap.xml (real fixture)",
        category: Category::RealWorld,
        xsd: xsd.into(),
        valid_xml: xml,
        invalid_xml: invalid,
    })
}

// ── scaling fixtures (TPC-H customer table) ─────────────────────────────────

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

fn synth_customer(rows: usize, label: &'static str) -> Fixture {
    let mut xml = String::from(r#"<table ID="customer">"#);
    let segments = ["AUTOMOBILE", "BUILDING", "FURNITURE", "HOUSEHOLD", "MACHINERY"];
    for i in 0..rows {
        xml.push_str(&format!(
            "<T><C_CUSTKEY>{i}</C_CUSTKEY>\
             <C_NAME>Customer #{i}</C_NAME>\
             <C_ADDRESS>{i} Some Street, Apt {}</C_ADDRESS>\
             <C_NATIONKEY>{}</C_NATIONKEY>\
             <C_PHONE>+1-555-{:04}-{:04}</C_PHONE>\
             <C_ACCTBAL>{}.{:02}</C_ACCTBAL>\
             <C_MKTSEGMENT>{}</C_MKTSEGMENT>\
             <C_COMMENT>regular customer notes for row {i}</C_COMMENT></T>",
             i % 100, i % 25,
             i % 10000, (i * 7) % 10000,
             (i as i64 * 13) % 99999, i % 100,
             segments[i % 5]));
    }
    xml.push_str("</table>");
    let invalid = xml.replacen("<C_MKTSEGMENT>AUTOMOBILE</C_MKTSEGMENT>",
                               "<C_MKTSEGMENT>BANKING</C_MKTSEGMENT>", 1);
    Fixture {
        label,
        category: Category::Scaling,
        xsd: CUSTOMER_XSD.into(),
        valid_xml: xml,
        invalid_xml: Some(invalid),
    }
}

fn customer_real() -> Option<Fixture> {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let xml = std::fs::read_to_string(format!("{manifest}/../../tests/assets/xml/customer1.xml")).ok()?;
    let invalid = xml.replacen("BUILDING", "BANKING", 1);
    Some(Fixture {
        label: "customer1.xml (1500 rows, real fixture)",
        category: Category::Scaling,
        xsd: CUSTOMER_XSD.into(),
        valid_xml: xml,
        invalid_xml: Some(invalid),
    })
}

// ── synthetic stress fixtures ───────────────────────────────────────────────

fn synth_wide_attrs() -> Fixture {
    // 20 attributes per element, each typed.  Tests attribute-decl
    // lookup and per-attribute validation overhead.
    let mut xsd = String::from(
        r#"<?xml version="1.0"?>
<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema">
  <xs:element name="rows">
    <xs:complexType>
      <xs:sequence>
        <xs:element name="r" maxOccurs="unbounded">
          <xs:complexType>
"#);
    for i in 0..20 {
        let ty = match i % 5 {
            0 => "xs:int", 1 => "xs:string", 2 => "xs:decimal",
            3 => "xs:boolean", _ => "xs:date",
        };
        xsd.push_str(&format!(
            r#"            <xs:attribute name="a{i}" type="{ty}" use="required"/>
"#));
    }
    xsd.push_str(
        r#"          </xs:complexType>
        </xs:element>
      </xs:sequence>
    </xs:complexType>
  </xs:element>
</xs:schema>"#);

    let mut xml = String::from("<rows>");
    for row in 0..500 {
        xml.push_str("<r");
        for i in 0..20 {
            let v: String = match i % 5 {
                0 => format!("{}", row * 7 + i),
                1 => format!("s{row}_{i}"),
                2 => format!("{}.{:02}", row + i, (row * 3) % 100),
                3 => (if (row + i) % 2 == 0 { "true" } else { "false" }).into(),
                _ => format!("20{:02}-{:02}-{:02}",
                             row % 25, (row % 12) + 1, (i % 28) + 1),
            };
            xml.push_str(&format!(" a{i}=\"{v}\""));
        }
        xml.push_str("/>");
    }
    xml.push_str("</rows>");
    // Break one boolean attribute.
    let invalid = xml.replacen(" a3=\"true\"", " a3=\"perhaps\"", 1);
    Fixture {
        label: "synth: wide attrs (500 elements × 20 attrs)",
        category: Category::Synthetic,
        xsd,
        valid_xml: xml,
        invalid_xml: Some(invalid),
    }
}

fn synth_deep_nest() -> Fixture {
    // 50 nested complex types, each holding the next.  Tests
    // type-chasing and per-level state overhead in the validator.
    const DEPTH: usize = 50;
    let mut xsd = String::from(
        r#"<?xml version="1.0"?>
<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema">
  <xs:element name="L0" type="T0"/>
"#);
    for i in 0..DEPTH {
        if i + 1 < DEPTH {
            xsd.push_str(&format!(
                r#"  <xs:complexType name="T{i}">
    <xs:sequence>
      <xs:element name="L{}" type="T{}" maxOccurs="unbounded"/>
    </xs:sequence>
  </xs:complexType>
"#, i + 1, i + 1));
        } else {
            xsd.push_str(&format!(
                r#"  <xs:complexType name="T{i}">
    <xs:sequence>
      <xs:element name="leaf" type="xs:int" minOccurs="0" maxOccurs="unbounded"/>
    </xs:sequence>
  </xs:complexType>
"#));
        }
    }
    xsd.push_str("</xs:schema>");

    // Build a single deeply-nested instance, with 10 leaf values at
    // the bottom — keeps the document small but every element walks
    // the full type chain.
    let mut xml = String::new();
    for i in 0..DEPTH - 1 {
        xml.push_str(&format!("<L{i}>"));
    }
    xml.push_str(&format!("<L{}>", DEPTH - 1));
    for v in 0..10 {
        xml.push_str(&format!("<leaf>{v}</leaf>"));
    }
    xml.push_str(&format!("</L{}>", DEPTH - 1));
    for i in (0..DEPTH - 1).rev() {
        xml.push_str(&format!("</L{i}>"));
    }
    let invalid = xml.replacen("<leaf>0</leaf>", "<leaf>not-an-int</leaf>", 1);
    Fixture {
        label: "synth: deep nesting (50 levels)",
        category: Category::Synthetic,
        xsd,
        valid_xml: xml,
        invalid_xml: Some(invalid),
    }
}

fn synth_enum_heavy() -> Fixture {
    // 100 enumeration values; 5000 element instances each carrying
    // one.  Hits the enumeration-membership path 5000× per validation.
    const N_ENUM: usize = 100;
    const N_ROWS: usize = 5000;
    let mut xsd = String::from(
        r#"<?xml version="1.0"?>
<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema">
  <xs:element name="set">
    <xs:complexType>
      <xs:sequence>
        <xs:element name="v" maxOccurs="unbounded">
          <xs:simpleType>
            <xs:restriction base="xs:string">
"#);
    for i in 0..N_ENUM {
        xsd.push_str(&format!(
            "              <xs:enumeration value=\"OPT_{i:03}\"/>\n"));
    }
    xsd.push_str(
        r#"            </xs:restriction>
          </xs:simpleType>
        </xs:element>
      </xs:sequence>
    </xs:complexType>
  </xs:element>
</xs:schema>"#);

    let mut xml = String::from("<set>");
    for i in 0..N_ROWS {
        xml.push_str(&format!("<v>OPT_{:03}</v>", i % N_ENUM));
    }
    xml.push_str("</set>");
    let invalid = xml.replacen("<v>OPT_000</v>", "<v>OPT_999</v>", 1);
    Fixture {
        label: "synth: enumeration (100 opts × 5000 picks)",
        category: Category::Synthetic,
        xsd,
        valid_xml: xml,
        invalid_xml: Some(invalid),
    }
}

fn synth_pattern_heavy() -> Fixture {
    // 5000 strings, each must match a non-trivial regex pattern.
    // Tests pattern-engine throughput.
    const N: usize = 5000;
    let xsd = r#"<?xml version="1.0"?>
<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema">
  <xs:element name="ids">
    <xs:complexType>
      <xs:sequence>
        <xs:element name="id" maxOccurs="unbounded">
          <xs:simpleType>
            <xs:restriction base="xs:string">
              <xs:pattern value="[A-Z]{3}-\d{4}-[a-f0-9]{8}"/>
            </xs:restriction>
          </xs:simpleType>
        </xs:element>
      </xs:sequence>
    </xs:complexType>
  </xs:element>
</xs:schema>"#;

    let mut xml = String::from("<ids>");
    for i in 0..N {
        let letters = [b'A' + (i % 26) as u8,
                       b'A' + ((i / 26) % 26) as u8,
                       b'A' + ((i / 676) % 26) as u8];
        let l = std::str::from_utf8(&letters).unwrap();
        // Pattern requires exactly 8 hex digits — mask to 32 bits.
        let suffix = (i.wrapping_mul(2654435761) & 0xFFFF_FFFF) as u32;
        xml.push_str(&format!("<id>{l}-{:04}-{:08x}</id>", i % 10000, suffix));
    }
    xml.push_str("</ids>");
    // First id will fail (too short for the suffix segment).
    let invalid = xml.replacen("<id>AAA-", "<id>aaa-", 1);
    Fixture {
        label: "synth: pattern (5000 regex matches)",
        category: Category::Synthetic,
        xsd: xsd.into(),
        valid_xml: xml,
        invalid_xml: Some(invalid),
    }
}

// ── on-disk fixture pair ────────────────────────────────────────────────────

fn xml_xsd_pair() -> Option<Fixture> {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let xsd = std::fs::read_to_string(format!("{manifest}/../../tests/assets/xml/xml_xsd_2.xml")).ok()?;
    let xml = std::fs::read_to_string(format!("{manifest}/../../tests/assets/xml/xml_xsd_1.xml")).ok()?;
    // Break a language-code enumeration (LNGC accepts G/F).
    let invalid = xml.replace("<LNGC>G</LNGC>", "<LNGC>X</LNGC>");
    let invalid = if invalid == xml { None } else { Some(invalid) };
    Some(Fixture {
        label: "xml_xsd pair (deeply nested inline types)",
        category: Category::FixturePair,
        xsd,
        valid_xml: xml,
        invalid_xml: invalid,
    })
}

// ── timing helpers ───────────────────────────────────────────────────────────

fn iters() -> usize {
    std::env::var("SUPXML_XSD_ITERS").ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(50)
}

/// Best-of-N wall time (min beats mean on a noisy machine because
/// scheduling jitter is one-sided).
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

fn ratio(a: u128, b: u128) -> String {
    if a == 0 || b == 0 { return "  n/a ".into(); }
    let r = a as f64 / b as f64;
    if r >= 1.0 { format!("{:.2}× slower", r) }
    else        { format!("{:.2}× faster", 1.0 / r) }
}

// ── per-fixture probe + timing ──────────────────────────────────────────────

/// All measurements for one fixture, in nanoseconds.  `None` means the
/// phase was skipped (validator can't handle the input).
#[derive(Default)]
struct Row {
    label:          String,
    xml_bytes:      usize,
    sx_compile:     Option<u128>,
    lx_compile:     Option<u128>,
    sx_validate:    Option<u128>,
    lx_validate:    Option<u128>,
    sx_reject:      Option<u128>,
    lx_reject:      Option<u128>,
    notes:          Vec<String>,
}

fn run_fixture(f: &Fixture, n: usize) -> Row {
    let mut row = Row {
        label:     f.label.into(),
        xml_bytes: f.valid_xml.len(),
        ..Default::default()
    };

    // Probe compile on both sides.
    let sx_schema = match Schema::compile_str(&f.xsd) {
        Ok(s)  => Some(s),
        Err(e) => { row.notes.push(format!("sup-xml compile failed: {e}")); None }
    };
    let lx_schema = unsafe { libxml2_silent_parse(f.xsd.as_bytes()) };
    if lx_schema.is_null() { row.notes.push("libxml2 compile failed".into()); }

    // Probe validity of the conforming instance: skip rows where either
    // side disagrees so the headline numbers stay honest.
    let sx_accepts = sx_schema.as_ref().is_some_and(|s| s.validate_str(&f.valid_xml).is_ok());
    let lx_accepts = if !lx_schema.is_null() {
        unsafe { libxml2_validate(lx_schema, f.valid_xml.as_bytes()) }
    } else { None };
    if sx_schema.is_some() && !sx_accepts {
        row.notes.push("sup-xml rejected the conforming instance".into());
    }
    if lx_accepts == Some(false) {
        row.notes.push("libxml2 rejected the conforming instance".into());
    }

    let do_validate_rows =
        sx_schema.is_some() && sx_accepts &&
        !lx_schema.is_null() && lx_accepts == Some(true);

    // Compile timings.
    if sx_schema.is_some() {
        row.sx_compile = Some(min_ns(n, || {
            let s = Schema::compile_str(&f.xsd).expect("re-compile");
            std::hint::black_box(s);
        }));
    }
    if !lx_schema.is_null() {
        row.lx_compile = Some(min_ns(n, || unsafe {
            let s = libxml2_silent_parse(f.xsd.as_bytes());
            assert!(!s.is_null());
            xmlSchemaFree(s);
        }));
    }

    // Validate timings (only when both validators accept the conforming
    // instance — otherwise the comparison is meaningless).
    if do_validate_rows {
        let s = sx_schema.as_ref().unwrap();
        row.sx_validate = Some(min_ns(n, || {
            s.validate_str(&f.valid_xml).expect("re-validate");
        }));
        row.lx_validate = Some(min_ns(n, || unsafe {
            let ok = libxml2_validate(lx_schema, f.valid_xml.as_bytes());
            assert_eq!(ok, Some(true));
        }));
    }

    // Reject timings (only when both validators reject the mutated
    // instance).
    if let (Some(inv), true) = (&f.invalid_xml, do_validate_rows) {
        let sx_rejects = sx_schema.as_ref().is_some_and(|s| s.validate_str(inv).is_err());
        let lx_rejects = unsafe { libxml2_validate(lx_schema, inv.as_bytes()) } == Some(false);
        if sx_rejects && lx_rejects {
            let s = sx_schema.as_ref().unwrap();
            row.sx_reject = Some(min_ns(n.max(20), || {
                let _ = s.validate_str(inv);
            }));
            row.lx_reject = Some(min_ns(n.max(20), || unsafe {
                let _ = libxml2_validate(lx_schema, inv.as_bytes());
            }));
        } else if sx_schema.is_some() {
            row.notes.push(format!(
                "skipped reject column: sup-xml-rejects={sx_rejects}, libxml2-rejects={lx_rejects}"));
        }
    }

    if !lx_schema.is_null() { unsafe { xmlSchemaFree(lx_schema); } }
    row
}

// ── output ──────────────────────────────────────────────────────────────────

fn fmt_throughput(ns: Option<u128>, bytes: usize) -> String {
    match ns {
        Some(ns) if ns > 0 => {
            let mb = bytes as f64 / (1024.0 * 1024.0);
            format!("{:>6.1} MB/s", mb / (ns as f64 / 1e9))
        }
        _ => "       —".into(),
    }
}

fn fmt_time(ns: Option<u128>) -> String {
    match ns { Some(ns) => format!("{:>10}", fmt_ns(ns)), None => "         —".into() }
}

fn print_category(cat: Category, rows: &[Row]) {
    println!("── {} ──", cat.header());
    println!("    {:<48} {:>10} {:>10} {:>14}  {:>10} {:>10} {:>14}  {:>10} {:>10} {:>14}",
        "fixture",
        "sx cmp", "lx cmp", "compile",
        "sx val", "lx val", "validate",
        "sx rej", "lx rej", "reject");
    for r in rows {
        // Validation throughput in MB/s is most readable when present;
        // print both absolute time + MB/s under it as a sub-line.
        println!("    {:<48} {} {} {:>14}  {} {} {:>14}  {} {} {:>14}",
            truncate(&r.label, 48),
            fmt_time(r.sx_compile), fmt_time(r.lx_compile),
            match (r.sx_compile, r.lx_compile) {
                (Some(s), Some(l)) => ratio(s, l), _ => "".into(),
            },
            fmt_time(r.sx_validate), fmt_time(r.lx_validate),
            match (r.sx_validate, r.lx_validate) {
                (Some(s), Some(l)) => ratio(s, l), _ => "".into(),
            },
            fmt_time(r.sx_reject), fmt_time(r.lx_reject),
            match (r.sx_reject, r.lx_reject) {
                (Some(s), Some(l)) => ratio(s, l), _ => "".into(),
            });
        if r.sx_validate.is_some() && r.lx_validate.is_some() {
            println!("    {:<48} {:>32}  {} {} {:>14}",
                "",
                format!("({} xml)", fmt_bytes(r.xml_bytes)),
                fmt_throughput(r.sx_validate, r.xml_bytes),
                fmt_throughput(r.lx_validate, r.xml_bytes),
                "");
        }
        for n in &r.notes {
            println!("        note: {n}");
        }
    }

    if let Some(summary) = geomean_row(rows) {
        println!("    {:<48} {:>10} {:>10} {:>14}  {:>10} {:>10} {:>14}  {:>10} {:>10} {:>14}",
            "geomean (sx vs lx)",
            "", "", summary.compile,
            "", "", summary.validate,
            "", "", summary.reject);
    }
    println!();
}

fn fmt_bytes(n: usize) -> String {
    if n >= 1024 * 1024 { format!("{:.1} MB", n as f64 / (1024.0 * 1024.0)) }
    else if n >= 1024   { format!("{:.0} KB", n as f64 / 1024.0) }
    else                { format!("{n} B") }
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n { s.into() } else { format!("{}…", &s[..n.saturating_sub(1)]) }
}

struct GeoSummary {
    compile:  String,
    validate: String,
    reject:   String,
}

fn geomean_row(rows: &[Row]) -> Option<GeoSummary> {
    fn gm(rows: &[Row], pick: impl Fn(&Row) -> (Option<u128>, Option<u128>)) -> String {
        let ratios: Vec<f64> = rows.iter().filter_map(|r| {
            let (s, l) = pick(r);
            match (s, l) {
                (Some(s), Some(l)) if s > 0 && l > 0 => Some(s as f64 / l as f64),
                _ => None,
            }
        }).collect();
        if ratios.is_empty() { return "  n/a ".into(); }
        let log_sum: f64 = ratios.iter().map(|r| r.ln()).sum();
        let g = (log_sum / ratios.len() as f64).exp();
        if g >= 1.0 { format!("{:.2}× slower", g) }
        else        { format!("{:.2}× faster", 1.0 / g) }
    }
    Some(GeoSummary {
        compile:  gm(rows, |r| (r.sx_compile,  r.lx_compile)),
        validate: gm(rows, |r| (r.sx_validate, r.lx_validate)),
        reject:   gm(rows, |r| (r.sx_reject,   r.lx_reject)),
    })
}

// ── main ─────────────────────────────────────────────────────────────────────

fn main() {
    install_libxml2_silencer();
    let n = iters();

    // Build every fixture, then probe and time each one.  Fixtures that
    // depend on on-disk files are filtered out when missing.
    let mut fixtures: Vec<Fixture> = vec![
        purchase_order(),
        atom_feed(),
        pom(),
        hr(),
        config(),
        event_log(),
    ];
    if let Some(f) = sitemap_real() { fixtures.push(f); }

    fixtures.push(synth_customer(100,   "synth: customer (100 rows)"));
    fixtures.push(synth_customer(1500,  "synth: customer (1500 rows)"));
    fixtures.push(synth_customer(10000, "synth: customer (10000 rows)"));
    if let Some(f) = customer_real() { fixtures.push(f); }

    fixtures.push(synth_wide_attrs());
    fixtures.push(synth_deep_nest());
    fixtures.push(synth_enum_heavy());
    fixtures.push(synth_pattern_heavy());

    if let Some(f) = xml_xsd_pair() { fixtures.push(f); }

    println!("\nXSD validator benchmark — sup_xml::xsd vs libxml2");
    println!("(min over {n} iterations; smaller = better; \"faster\" = sup_xml wins)\n");

    let mut all_rows: Vec<Row> = Vec::with_capacity(fixtures.len());
    let cats = [
        Category::RealWorld,
        Category::Scaling,
        Category::Synthetic,
        Category::FixturePair,
    ];
    for &cat in &cats {
        let mut cat_rows: Vec<Row> = Vec::new();
        for f in fixtures.iter().filter(|f| f.category == cat) {
            cat_rows.push(run_fixture(f, n));
        }
        if cat_rows.is_empty() { continue; }
        print_category(cat, &cat_rows);
        all_rows.extend(cat_rows);
    }

    // Overall summary across every fixture, all phases.
    if let Some(s) = geomean_row(&all_rows) {
        println!("── overall geomean across all categories ──");
        println!("    compile : {}", s.compile);
        println!("    validate: {}", s.validate);
        println!("    reject  : {}", s.reject);
        println!();
    }
}
