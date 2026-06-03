//! Real-world XSD/XML pairs.  Each test pairs a schema modelled on an
//! actual format with a valid instance and one or more deliberately
//! invalid instances, exercising the validator end-to-end.
//!
//! Schemas are flattened into single files (no `<xs:import>` /
//! `<xs:include>` until PR4 wires the resolver), but element shapes,
//! datatypes, and constraints match the original spec text.

#![cfg(feature = "xsd")]

use sup_xml::xsd::Schema;

// ── 1.  Purchase Order ──────────────────────────────────────────────────────
//
// The canonical XSD example from the W3C XML Schema Primer.  Exercises:
//   * elements with attributes
//   * nested sequences
//   * a custom simpleType with restriction (USState enumeration)
//   * decimal with totalDigits / fractionDigits
//   * date type
//   * unbounded repetition

const PO_XSD: &str = r#"<?xml version="1.0"?>
<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema"
           targetNamespace="urn:po"
           xmlns="urn:po"
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
      <xs:element name="comment"     type="xs:string" minOccurs="0"/>
    </xs:sequence>
    <xs:attribute name="partNum" type="xs:string" use="required"/>
  </xs:complexType>

</xs:schema>"#;

#[test]
fn purchase_order_valid() {
    let s = Schema::compile_str(PO_XSD).unwrap();
    let xml = r#"<?xml version="1.0"?>
<purchaseOrder xmlns="urn:po" orderDate="2024-03-15">
  <shipTo country="US">
    <name>Alice Roberts</name>
    <street>123 Main St</street>
    <city>Boston</city>
    <state>MA</state>
    <zip>02108</zip>
  </shipTo>
  <billTo country="US">
    <name>Acme Corp</name>
    <street>1 Industrial Way</street>
    <city>Albany</city>
    <state>NY</state>
    <zip>12207</zip>
  </billTo>
  <comment>Please rush.</comment>
  <items>
    <item partNum="WIDGET-1">
      <productName>Widget</productName>
      <quantity>3</quantity>
      <USPrice>9.99</USPrice>
    </item>
    <item partNum="SPROCKET-7">
      <productName>Sprocket</productName>
      <quantity>1</quantity>
      <USPrice>29.50</USPrice>
      <comment>Engraved.</comment>
    </item>
  </items>
</purchaseOrder>"#;
    s.validate_str(xml).unwrap();
}

#[test]
fn purchase_order_rejects_invalid_state() {
    let s = Schema::compile_str(PO_XSD).unwrap();
    let xml = r#"<purchaseOrder xmlns="urn:po" orderDate="2024-03-15">
  <shipTo country="US">
    <name>X</name><street>X</street><city>X</city>
    <state>XX</state>
    <zip>00000</zip>
  </shipTo>
  <billTo country="US">
    <name>X</name><street>X</street><city>X</city>
    <state>NY</state><zip>00000</zip>
  </billTo>
  <items>
    <item partNum="A">
      <productName>P</productName><quantity>1</quantity><USPrice>1.00</USPrice>
    </item>
  </items>
</purchaseOrder>"#;
    let err = s.validate_str(xml).unwrap_err();
    assert!(err.issues.iter().any(|i| i.message.contains("enumeration")));
}

#[test]
fn purchase_order_rejects_quantity_over_100() {
    let s = Schema::compile_str(PO_XSD).unwrap();
    let xml = r#"<purchaseOrder xmlns="urn:po" orderDate="2024-03-15">
  <shipTo country="US">
    <name>X</name><street>X</street><city>X</city><state>NY</state><zip>0</zip>
  </shipTo>
  <billTo country="US">
    <name>X</name><street>X</street><city>X</city><state>NY</state><zip>0</zip>
  </billTo>
  <items>
    <item partNum="A">
      <productName>P</productName>
      <quantity>200</quantity>
      <USPrice>1.00</USPrice>
    </item>
  </items>
</purchaseOrder>"#;
    let err = s.validate_str(xml).unwrap_err();
    assert!(err.issues.iter().any(|i| i.message.contains("maxExclusive")));
}

#[test]
fn purchase_order_rejects_bad_date() {
    let s = Schema::compile_str(PO_XSD).unwrap();
    let xml = r#"<purchaseOrder xmlns="urn:po" orderDate="not-a-date">
  <shipTo country="US">
    <name>X</name><street>X</street><city>X</city><state>NY</state><zip>0</zip>
  </shipTo>
  <billTo country="US">
    <name>X</name><street>X</street><city>X</city><state>NY</state><zip>0</zip>
  </billTo>
  <items>
    <item partNum="A">
      <productName>P</productName><quantity>1</quantity><USPrice>1.00</USPrice>
    </item>
  </items>
</purchaseOrder>"#;
    let err = s.validate_str(xml).unwrap_err();
    assert!(err.issues.iter().any(|i|
        i.message.contains("orderDate") || i.message.to_lowercase().contains("date")
    ));
}

// ── 2.  Atom 1.0 feed (subset) ──────────────────────────────────────────────
//
// RFC 4287 publishing format.  Exercises:
//   * choice content (text / html / xhtml content types)
//   * dateTime values
//   * anyURI fields
//   * unbounded repeated elements

const ATOM_XSD: &str = r#"<?xml version="1.0"?>
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

#[test]
fn atom_feed_valid() {
    let s = Schema::compile_str(ATOM_XSD).unwrap();
    let xml = r#"<?xml version="1.0"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <title>Example Feed</title>
  <id>https://example.org/feed</id>
  <updated>2024-03-15T10:00:00Z</updated>
  <link href="https://example.org/" rel="alternate"/>
  <author>
    <name>Alice</name>
    <uri>https://example.org/~alice</uri>
  </author>
  <entry>
    <title type="html">First &amp; foremost</title>
    <id>https://example.org/posts/1</id>
    <updated>2024-03-15T10:30:00Z</updated>
    <summary>A short summary.</summary>
  </entry>
  <entry>
    <title>Second post</title>
    <id>https://example.org/posts/2</id>
    <updated>2024-03-15T11:00:00Z</updated>
    <published>2024-03-14T09:00:00Z</published>
  </entry>
</feed>"#;
    s.validate_str(xml).unwrap();
}

#[test]
fn atom_rejects_invalid_datetime() {
    let s = Schema::compile_str(ATOM_XSD).unwrap();
    let xml = r#"<feed xmlns="http://www.w3.org/2005/Atom">
  <title>X</title>
  <id>https://example.org/</id>
  <updated>yesterday</updated>
</feed>"#;
    assert!(s.validate_str(xml).is_err());
}

#[test]
fn atom_rejects_unknown_text_type() {
    let s = Schema::compile_str(ATOM_XSD).unwrap();
    let xml = r#"<feed xmlns="http://www.w3.org/2005/Atom">
  <title type="markdown">X</title>
  <id>https://example.org/</id>
  <updated>2024-01-01T00:00:00Z</updated>
</feed>"#;
    let err = s.validate_str(xml).unwrap_err();
    assert!(err.issues.iter().any(|i| i.message.contains("enumeration")));
}

// ── 3.  Maven-style dependency descriptor ───────────────────────────────────
//
// Build-tooling shape — common in JVM/.NET ecosystems.  Exercises:
//   * deeply nested sequences
//   * optional vs. required fields
//   * id-style strings with regex (groupId/artifactId conventions)
//   * version with semver-style pattern

const POM_XSD: &str = r#"<?xml version="1.0"?>
<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema"
           targetNamespace="urn:pom"
           xmlns="urn:pom"
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
      <xs:element name="scope"      minOccurs="0">
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

#[test]
fn pom_valid() {
    let s = Schema::compile_str(POM_XSD).unwrap();
    let xml = r#"<?xml version="1.0"?>
<project xmlns="urn:pom">
  <modelVersion>4.0.0</modelVersion>
  <groupId>org.example</groupId>
  <artifactId>my-app</artifactId>
  <version>1.2.3</version>
  <packaging>jar</packaging>
  <dependencies>
    <dependency>
      <groupId>junit</groupId>
      <artifactId>junit</artifactId>
      <version>4.13.2</version>
      <scope>test</scope>
    </dependency>
    <dependency>
      <groupId>org.slf4j</groupId>
      <artifactId>slf4j-api</artifactId>
      <version>2.0.9</version>
    </dependency>
  </dependencies>
</project>"#;
    s.validate_str(xml).unwrap();
}

#[test]
fn pom_rejects_uppercase_group_id() {
    let s = Schema::compile_str(POM_XSD).unwrap();
    let xml = r#"<project xmlns="urn:pom">
  <modelVersion>4.0.0</modelVersion>
  <groupId>OrgExample</groupId>
  <artifactId>my-app</artifactId>
  <version>1.0.0</version>
  <packaging>jar</packaging>
</project>"#;
    let err = s.validate_str(xml).unwrap_err();
    assert!(err.issues.iter().any(|i| i.message.contains("pattern")));
}

#[test]
fn pom_rejects_bad_semver() {
    let s = Schema::compile_str(POM_XSD).unwrap();
    let xml = r#"<project xmlns="urn:pom">
  <modelVersion>4.0.0</modelVersion>
  <groupId>org.example</groupId>
  <artifactId>app</artifactId>
  <version>v1</version>
  <packaging>jar</packaging>
</project>"#;
    let err = s.validate_str(xml).unwrap_err();
    assert!(err.issues.iter().any(|i| i.message.contains("pattern")));
}

#[test]
fn pom_rejects_wrong_model_version() {
    let s = Schema::compile_str(POM_XSD).unwrap();
    let xml = r#"<project xmlns="urn:pom">
  <modelVersion>5.0.0</modelVersion>
  <groupId>org.example</groupId>
  <artifactId>app</artifactId>
  <version>1.0.0</version>
  <packaging>jar</packaging>
</project>"#;
    let err = s.validate_str(xml).unwrap_err();
    assert!(err.issues.iter().any(|i| i.message.contains("fixed")));
}

// ── 4.  RSS-style channel ───────────────────────────────────────────────────
//
// Mirrors RSS 2.0 channel/item shape (in no namespace, like the original).
// Exercises:
//   * no-namespace schemas
//   * choice between author/dc:creator (modelled as choice)
//   * mixed types (email, URI)

const RSS_XSD: &str = r#"<?xml version="1.0"?>
<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema">

  <xs:element name="rss" type="RssType"/>

  <xs:complexType name="RssType">
    <xs:sequence>
      <xs:element name="channel" type="ChannelType"/>
    </xs:sequence>
    <xs:attribute name="version" type="xs:string" use="required" fixed="2.0"/>
  </xs:complexType>

  <xs:complexType name="ChannelType">
    <xs:sequence>
      <xs:element name="title"       type="xs:string"/>
      <xs:element name="link"        type="xs:anyURI"/>
      <xs:element name="description" type="xs:string"/>
      <xs:element name="language"    type="xs:language" minOccurs="0"/>
      <xs:element name="pubDate"     type="xs:string"   minOccurs="0"/>
      <xs:element name="item"        type="ItemType"    minOccurs="0" maxOccurs="unbounded"/>
    </xs:sequence>
  </xs:complexType>

  <xs:complexType name="ItemType">
    <xs:sequence>
      <xs:element name="title"       type="xs:string" minOccurs="0"/>
      <xs:element name="link"        type="xs:anyURI" minOccurs="0"/>
      <xs:element name="description" type="xs:string" minOccurs="0"/>
      <xs:element name="author"      type="xs:string" minOccurs="0"/>
      <xs:element name="guid"        type="xs:string" minOccurs="0"/>
      <xs:element name="pubDate"     type="xs:string" minOccurs="0"/>
    </xs:sequence>
  </xs:complexType>

</xs:schema>"#;

#[test]
fn rss_valid() {
    let s = Schema::compile_str(RSS_XSD).unwrap();
    let xml = r#"<?xml version="1.0"?>
<rss version="2.0">
  <channel>
    <title>Example News</title>
    <link>https://example.org/</link>
    <description>A demo feed.</description>
    <language>en-US</language>
    <item>
      <title>First Post</title>
      <link>https://example.org/posts/1</link>
      <description>Hello.</description>
      <guid>tag:example.org,2024:1</guid>
    </item>
    <item>
      <title>Second Post</title>
      <link>https://example.org/posts/2</link>
      <description>Hi again.</description>
    </item>
  </channel>
</rss>"#;
    s.validate_str(xml).unwrap();
}

#[test]
fn rss_rejects_wrong_version_attribute() {
    let s = Schema::compile_str(RSS_XSD).unwrap();
    let xml = r#"<rss version="3.0">
  <channel>
    <title>X</title>
    <link>https://example.org/</link>
    <description>X</description>
  </channel>
</rss>"#;
    let err = s.validate_str(xml).unwrap_err();
    assert!(err.issues.iter().any(|i| i.message.contains("fixed")));
}

#[test]
fn rss_rejects_missing_required_channel_field() {
    let s = Schema::compile_str(RSS_XSD).unwrap();
    let xml = r#"<rss version="2.0">
  <channel>
    <title>X</title>
    <link>https://example.org/</link>
  </channel>
</rss>"#;
    let err = s.validate_str(xml).unwrap_err();
    assert!(err.issues.iter().any(|i| i.message.contains("description")));
}

// ── 5.  HR record / employee data ───────────────────────────────────────────
//
// Common XML shape for HR/payroll integration.  Exercises:
//   * dates and decimals
//   * NMTOKEN-typed identifiers
//   * minInclusive/maxInclusive on numeric types
//   * choice between employee statuses
//   * deeply-nested address records

const HR_XSD: &str = r#"<?xml version="1.0"?>
<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema"
           targetNamespace="urn:hr"
           xmlns="urn:hr"
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
      <xs:element name="address" type="AddressType" minOccurs="0"/>
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

  <xs:complexType name="AddressType">
    <xs:sequence>
      <xs:element name="street" type="xs:string"/>
      <xs:element name="city"   type="xs:string"/>
      <xs:element name="zip"    type="xs:string"/>
    </xs:sequence>
  </xs:complexType>

</xs:schema>"#;

#[test]
fn hr_valid() {
    let s = Schema::compile_str(HR_XSD).unwrap();
    let xml = r#"<?xml version="1.0"?>
<employees xmlns="urn:hr">
  <employee empId="E000123">
    <firstName>Ada</firstName>
    <lastName>Lovelace</lastName>
    <hireDate>2018-04-12</hireDate>
    <salary>120000.00</salary>
    <status>active</status>
    <address>
      <street>1 Analytical Way</street>
      <city>London</city>
      <zip>EC1A 1BB</zip>
    </address>
  </employee>
  <employee empId="E000456">
    <firstName>Grace</firstName>
    <lastName>Hopper</lastName>
    <hireDate>2019-09-01</hireDate>
    <salary>135000.50</salary>
    <status>leave</status>
  </employee>
</employees>"#;
    s.validate_str(xml).unwrap();
}

#[test]
fn hr_rejects_bad_emp_id_format() {
    let s = Schema::compile_str(HR_XSD).unwrap();
    let xml = r#"<employees xmlns="urn:hr">
  <employee empId="ABC">
    <firstName>X</firstName><lastName>X</lastName>
    <hireDate>2024-01-01</hireDate>
    <salary>1000.00</salary>
    <status>active</status>
  </employee>
</employees>"#;
    let err = s.validate_str(xml).unwrap_err();
    assert!(err.issues.iter().any(|i|
        i.message.contains("pattern") || i.message.contains("empId")));
}

#[test]
fn hr_rejects_negative_salary() {
    let s = Schema::compile_str(HR_XSD).unwrap();
    let xml = r#"<employees xmlns="urn:hr">
  <employee empId="E000001">
    <firstName>X</firstName><lastName>X</lastName>
    <hireDate>2024-01-01</hireDate>
    <salary>-500.00</salary>
    <status>active</status>
  </employee>
</employees>"#;
    let err = s.validate_str(xml).unwrap_err();
    assert!(err.issues.iter().any(|i| i.message.contains("minInclusive")));
}

#[test]
fn hr_rejects_unknown_status() {
    let s = Schema::compile_str(HR_XSD).unwrap();
    let xml = r#"<employees xmlns="urn:hr">
  <employee empId="E000001">
    <firstName>X</firstName><lastName>X</lastName>
    <hireDate>2024-01-01</hireDate>
    <salary>1000.00</salary>
    <status>retired</status>
  </employee>
</employees>"#;
    let err = s.validate_str(xml).unwrap_err();
    assert!(err.issues.iter().any(|i| i.message.contains("enumeration")));
}

#[test]
fn hr_rejects_invalid_date() {
    let s = Schema::compile_str(HR_XSD).unwrap();
    let xml = r#"<employees xmlns="urn:hr">
  <employee empId="E000001">
    <firstName>X</firstName><lastName>X</lastName>
    <hireDate>2024-02-31</hireDate>
    <salary>1000.00</salary>
    <status>active</status>
  </employee>
</employees>"#;
    let err = s.validate_str(xml).unwrap_err();
    assert!(err.issues.iter().any(|i|
        i.message.to_lowercase().contains("date") || i.message.contains("day")
    ));
}

// ── 6.  Server / TLS configuration shape ────────────────────────────────────
//
// A typical app-config.xml.  Exercises:
//   * port range validation (unsignedShort with maxInclusive)
//   * boolean
//   * choice for log destination
//   * default attribute values

const CONFIG_XSD: &str = r#"<?xml version="1.0"?>
<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema"
           targetNamespace="urn:cfg"
           xmlns="urn:cfg"
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
      <xs:element name="metrics"  type="xs:boolean" minOccurs="0"/>
      <xs:element name="profiling" type="xs:boolean" minOccurs="0"/>
    </xs:sequence>
  </xs:complexType>

</xs:schema>"#;

#[test]
fn config_valid() {
    let s = Schema::compile_str(CONFIG_XSD).unwrap();
    let xml = r#"<?xml version="1.0"?>
<config xmlns="urn:cfg">
  <server>
    <host>0.0.0.0</host>
    <port>8443</port>
    <tls>true</tls>
  </server>
  <logging>
    <level>info</level>
    <file>/var/log/app.log</file>
  </logging>
  <features>
    <metrics>true</metrics>
  </features>
</config>"#;
    s.validate_str(xml).unwrap();
}

#[test]
fn config_valid_with_syslog_choice() {
    let s = Schema::compile_str(CONFIG_XSD).unwrap();
    let xml = r#"<config xmlns="urn:cfg">
  <server>
    <host>localhost</host>
    <port>80</port>
    <tls>false</tls>
  </server>
  <logging>
    <level>debug</level>
    <syslog facility="local3"/>
  </logging>
</config>"#;
    s.validate_str(xml).unwrap();
}

#[test]
fn config_rejects_port_out_of_range() {
    let s = Schema::compile_str(CONFIG_XSD).unwrap();
    let xml = r#"<config xmlns="urn:cfg">
  <server>
    <host>x</host>
    <port>99999</port>
    <tls>true</tls>
  </server>
  <logging>
    <level>info</level>
    <stdout/>
  </logging>
</config>"#;
    let err = s.validate_str(xml).unwrap_err();
    assert!(err.issues.iter().any(|i|
        i.message.contains("unsignedShort")
        || i.message.contains("maxInclusive")
        || i.message.contains("range")
    ));
}

#[test]
fn config_rejects_two_log_destinations() {
    let s = Schema::compile_str(CONFIG_XSD).unwrap();
    // logging.choice means *one* of file / syslog / stdout — declaring
    // two should fail.
    let xml = r#"<config xmlns="urn:cfg">
  <server><host>x</host><port>80</port><tls>false</tls></server>
  <logging>
    <level>info</level>
    <file>/tmp/log</file>
    <stdout/>
  </logging>
</config>"#;
    assert!(s.validate_str(xml).is_err());
}

#[test]
fn config_rejects_unknown_log_level() {
    let s = Schema::compile_str(CONFIG_XSD).unwrap();
    let xml = r#"<config xmlns="urn:cfg">
  <server><host>x</host><port>80</port><tls>false</tls></server>
  <logging>
    <level>ULTRAVERBOSE</level>
    <stdout/>
  </logging>
</config>"#;
    let err = s.validate_str(xml).unwrap_err();
    assert!(err.issues.iter().any(|i| i.message.contains("enumeration")));
}

// ── 7.  Real fixture: tests/assets/xml/sitemap.xml ──────────────────────────
//
// sitemaps.org schema, simplified to the elements present in the
// fixture (no `<image:image>` extensions).

const SITEMAP_XSD: &str = r#"<?xml version="1.0"?>
<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema"
           targetNamespace="http://www.sitemaps.org/schemas/sitemap/0.9"
           elementFormDefault="qualified"
           xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">

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
            <xs:enumeration value="always"/>
            <xs:enumeration value="hourly"/>
            <xs:enumeration value="daily"/>
            <xs:enumeration value="weekly"/>
            <xs:enumeration value="monthly"/>
            <xs:enumeration value="yearly"/>
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

#[test]
fn sitemap_real_fixture_validates() {
    let s = Schema::compile_str(SITEMAP_XSD).unwrap();
    let xml = std::fs::read_to_string(
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/assets/xml/sitemap.xml")
    ).expect("fixture sitemap.xml");
    s.validate_str(&xml).expect("sitemap.xml should validate");
}

// ── 8.  Real fixture: tests/assets/xml/customer1.xml ────────────────────────
//
// TPC-H benchmark customer table — 1500 records.  Stress-tests
// validation throughput across a long stream of identical-shape rows.

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
      <xs:element name="C_CUSTKEY"    type="xs:int"/>
      <xs:element name="C_NAME"       type="xs:string"/>
      <xs:element name="C_ADDRESS"    type="xs:string"/>
      <xs:element name="C_NATIONKEY"  type="xs:int"/>
      <xs:element name="C_PHONE"      type="xs:string"/>
      <xs:element name="C_ACCTBAL"    type="xs:decimal"/>
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
      <xs:element name="C_COMMENT"    type="xs:string"/>
    </xs:sequence>
  </xs:complexType>

</xs:schema>"#;

#[test]
fn customer_real_fixture_validates() {
    let s = Schema::compile_str(CUSTOMER_XSD).unwrap();
    let xml = std::fs::read_to_string(
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/assets/xml/customer1.xml")
    ).expect("fixture customer1.xml");
    s.validate_str(&xml).expect("customer1.xml should validate");
}

#[test]
fn customer_fixture_catches_synthetic_breakage() {
    // Same schema, but mutate a value to ensure the validator actually
    // looks at the data — flip a market segment to one not in the
    // enumeration.
    let s = Schema::compile_str(CUSTOMER_XSD).unwrap();
    let xml = std::fs::read_to_string(
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/assets/xml/customer1.xml")
    ).unwrap();
    let mutated = xml.replacen("BUILDING", "BANKING", 1);
    let err = s.validate_str(&mutated).unwrap_err();
    assert!(err.issues.iter().any(|i| i.message.contains("enumeration")));
}

// ── 9.  Real fixture pair: xml_xsd_1.xml + xml_xsd_2.xml ────────────────────
//
// A B2B purchase order in the schema-and-instance pair already on disk.
// xml_xsd_2.xml is the XSD; xml_xsd_1.xml is a conforming order
// (Swiss/German style, with `dd.mm.yyyy` dates as plain `xs:string`).
//
// Exercises:
//   * deeply nested inline `<xs:complexType>` declarations (no
//     top-level type names anywhere in the body)
//   * many named simple types (`String10`, `String50`, …) referenced
//     by name from element decls
//   * a restriction whose base is a user-defined simple type (`String1`)
//     rather than a built-in
//   * `xs:annotation` / `xs:documentation` everywhere — must be ignored
//   * inline `<xs:simpleType>` inside attribute decls
//   * `xs:enumeration` constraints (LNGC: G/F, SHCF: Y/N/M)
//   * `xs:integer` / `xs:int` / `xs:double` / `xs:string`-derived facets

#[test]
fn xml_xsd_pair_validates() {
    let xsd = std::fs::read_to_string(
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/assets/xml/xml_xsd_2.xml")
    ).expect("xml_xsd_2.xml fixture missing");
    let xml = std::fs::read_to_string(
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/assets/xml/xml_xsd_1.xml")
    ).expect("xml_xsd_1.xml fixture missing");

    let schema = Schema::compile_str(&xsd)
        .expect("xml_xsd_2.xml schema should compile");
    schema.validate_str(&xml)
        .expect("xml_xsd_1.xml order document should validate");
}

#[test]
fn xml_xsd_pair_rejects_bad_language_code() {
    let xsd = std::fs::read_to_string(
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/assets/xml/xml_xsd_2.xml")
    ).unwrap();
    let xml = std::fs::read_to_string(
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/assets/xml/xml_xsd_1.xml")
    ).unwrap();
    let mutated = xml.replace("<LNGC>G</LNGC>", "<LNGC>X</LNGC>");
    let schema = Schema::compile_str(&xsd).unwrap();
    let err = schema.validate_str(&mutated).unwrap_err();
    assert!(err.issues.iter().any(|i| i.message.contains("enumeration")),
        "expected enumeration error, got {:?}", err.issues);
}

#[test]
fn xml_xsd_pair_rejects_quantity_below_one() {
    // OQTY has minInclusive=1 — try 0.
    let xsd = std::fs::read_to_string(
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/assets/xml/xml_xsd_2.xml")
    ).unwrap();
    let xml = std::fs::read_to_string(
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/assets/xml/xml_xsd_1.xml")
    ).unwrap();
    let mutated = xml.replacen("<OQTY>1</OQTY>", "<OQTY>0</OQTY>", 1);
    let schema = Schema::compile_str(&xsd).unwrap();
    let err = schema.validate_str(&mutated).unwrap_err();
    assert!(err.issues.iter().any(|i|
        i.message.contains("minInclusive") || i.message.contains("OQTY")
    ), "expected minInclusive error, got {:?}", err.issues);
}

#[test]
fn xml_xsd_pair_rejects_wrong_message_type() {
    // message_type is fixed="order".
    let xsd = std::fs::read_to_string(
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/assets/xml/xml_xsd_2.xml")
    ).unwrap();
    let xml = std::fs::read_to_string(
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/assets/xml/xml_xsd_1.xml")
    ).unwrap();
    let mutated = xml.replace(r#"message_type="order""#, r#"message_type="invoice""#);
    let schema = Schema::compile_str(&xsd).unwrap();
    let err = schema.validate_str(&mutated).unwrap_err();
    assert!(err.issues.iter().any(|i|
        i.message.contains("fixed") || i.message.contains("message_type")
    ), "expected fixed-value error, got {:?}", err.issues);
}

// ── 6.  Event log: polymorphism via substitution groups + xsi:type ──────────
//
// A real-world shape inspired by SAML 2.0 / CloudEvents / audit logs:
// `Event` is an abstract base type with concrete subtypes (LoginEvent,
// LogoutEvent, ErrorEvent).  Instances polymorph either through
// substitutionGroup (preferred in XSD-only environments) or xsi:type
// (preferred when emitting the head element name is mandatory).
//
// Exercises:
//   * abstract complex types
//   * substitution groups with abstract heads
//   * xsi:type substitution with content merge from extension chain
//   * identity constraints (unique event ids) across heterogeneous members
//   * required + optional attributes inherited via complex extension

const EVENT_LOG_XSD: &str = r#"<?xml version="1.0"?>
<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema"
           targetNamespace="urn:eventlog"
           xmlns="urn:eventlog"
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
    <xs:unique name="uniqueIds">
      <xs:selector xpath=".//login | .//logout | .//error"/>
      <xs:field xpath="@id"/>
    </xs:unique>
  </xs:element>
</xs:schema>
"#;

const EVENT_LOG_INSTANCE: &str = r#"<log xmlns="urn:eventlog">
  <login id="e1" source="web">
    <timestamp>2026-05-16T08:00:00Z</timestamp>
    <user>alice</user>
    <ip>10.0.0.1</ip>
  </login>
  <login id="e2">
    <timestamp>2026-05-16T08:15:00Z</timestamp>
    <user>bob</user>
    <ip>10.0.0.2</ip>
  </login>
  <error id="e3" source="api">
    <timestamp>2026-05-16T08:30:00Z</timestamp>
    <code>500</code>
    <message>Internal Server Error</message>
  </error>
  <logout id="e4">
    <timestamp>2026-05-16T09:00:00Z</timestamp>
    <user>alice</user>
  </logout>
</log>
"#;

#[test]
fn event_log_substitution_groups_validate() {
    let s = Schema::compile_str(EVENT_LOG_XSD).unwrap();
    s.validate_str(EVENT_LOG_INSTANCE).unwrap();
}

#[test]
fn event_log_xsi_type_substitution_validates() {
    let s = Schema::compile_str(EVENT_LOG_XSD).unwrap();
    let instance = r#"<log xmlns="urn:eventlog"
                          xmlns:el="urn:eventlog"
                          xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
        <event xsi:type="el:LoginEvent" id="e1" source="web">
            <timestamp>2026-05-16T08:00:00Z</timestamp>
            <user>alice</user>
            <ip>10.0.0.1</ip>
        </event>
        <event xsi:type="el:ErrorEvent" id="e2">
            <timestamp>2026-05-16T08:30:00Z</timestamp>
            <code>500</code>
            <message>boom</message>
        </event>
    </log>"#;
    s.validate_str(instance).unwrap();
}

#[test]
fn event_log_rejects_duplicate_ids_across_event_types() {
    let s = Schema::compile_str(EVENT_LOG_XSD).unwrap();
    let bad = r#"<log xmlns="urn:eventlog">
        <login id="dup"><timestamp>2026-05-16T08:00:00Z</timestamp>
            <user>a</user><ip>1.2.3.4</ip></login>
        <logout id="dup"><timestamp>2026-05-16T09:00:00Z</timestamp>
            <user>a</user></logout>
    </log>"#;
    let err = s.validate_str(bad).unwrap_err();
    assert!(err.issues.iter().any(|i|
        matches!(i.kind, sup_xml::xsd::ValidationKind::KeyNotUnique)
    ), "expected KeyNotUnique across substitution-group members, got {:?}", err.issues);
}

#[test]
fn event_log_rejects_extension_field_missing() {
    let s = Schema::compile_str(EVENT_LOG_XSD).unwrap();
    let bad = r#"<log xmlns="urn:eventlog">
        <login id="e1"><timestamp>2026-05-16T08:00:00Z</timestamp>
            <user>alice</user>
        </login>
    </log>"#;
    let err = s.validate_str(bad).unwrap_err();
    assert!(err.issues.iter().any(|i|
        matches!(i.kind, sup_xml::xsd::ValidationKind::MissingRequiredElement)
            && i.message.contains("ip")
    ), "expected MissingRequiredElement for <ip>, got {:?}", err.issues);
}

#[test]
fn event_log_rejects_inherited_attribute_missing() {
    let s = Schema::compile_str(EVENT_LOG_XSD).unwrap();
    let bad = r#"<log xmlns="urn:eventlog">
        <login><timestamp>2026-05-16T08:00:00Z</timestamp>
            <user>a</user><ip>1.2.3.4</ip></login>
    </log>"#;
    let err = s.validate_str(bad).unwrap_err();
    assert!(err.issues.iter().any(|i|
        matches!(i.kind, sup_xml::xsd::ValidationKind::MissingRequiredAttribute)
            && i.message.contains("id")
    ), "expected missing inherited @id, got {:?}", err.issues);
}

#[test]
fn event_log_rejects_wrong_xsi_type() {
    let s = Schema::compile_str(EVENT_LOG_XSD).unwrap();
    // Bind the xs: prefix so xsi:type="xs:string" resolves to a real
    // type — the derivation check should then reject it (xs:string
    // doesn't derive from Event).
    let bad = r#"<log xmlns="urn:eventlog"
                       xmlns:xs="http://www.w3.org/2001/XMLSchema"
                       xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
        <event xsi:type="xs:string" id="e1">hello</event>
    </log>"#;
    let err = s.validate_str(bad).unwrap_err();
    assert!(err.issues.iter().any(|i| i.message.contains("does not derive")),
        "expected xsi:type derivation rejection, got {:?}", err.issues);
}

// ── 7.  Catalog: identity constraints + xs:list + deep restrictions ─────────

const CATALOG_XSD: &str = r#"<?xml version="1.0"?>
<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema"
           targetNamespace="urn:catalog"
           xmlns="urn:catalog"
           elementFormDefault="qualified">

  <xs:simpleType name="Isbn">
    <xs:restriction base="xs:string">
      <xs:length value="13"/>
      <xs:pattern value="[0-9]{13}"/>
    </xs:restriction>
  </xs:simpleType>

  <xs:simpleType name="CategoryToken">
    <xs:restriction base="xs:string">
      <xs:pattern value="[a-z][a-z0-9-]*"/>
    </xs:restriction>
  </xs:simpleType>

  <xs:simpleType name="CategoryList">
    <xs:list itemType="CategoryToken"/>
  </xs:simpleType>

  <xs:simpleType name="LimitedCategoryList">
    <xs:restriction base="CategoryList">
      <xs:maxLength value="5"/>
    </xs:restriction>
  </xs:simpleType>

  <xs:complexType name="Book">
    <xs:sequence>
      <xs:element name="title"      type="xs:string"/>
      <xs:element name="categories" type="LimitedCategoryList"/>
      <xs:element name="year"       type="xs:gYear" minOccurs="0"/>
    </xs:sequence>
    <xs:attribute name="isbn" type="Isbn" use="required"/>
  </xs:complexType>

  <xs:complexType name="Order">
    <xs:attribute name="who"  type="xs:string" use="required"/>
    <xs:attribute name="isbn" type="Isbn"      use="required"/>
  </xs:complexType>

  <xs:element name="catalog">
    <xs:complexType>
      <xs:sequence>
        <xs:element name="books">
          <xs:complexType>
            <xs:sequence>
              <xs:element name="book" type="Book" maxOccurs="unbounded"/>
            </xs:sequence>
          </xs:complexType>
        </xs:element>
        <xs:element name="orders">
          <xs:complexType>
            <xs:sequence>
              <xs:element name="order" type="Order" maxOccurs="unbounded" minOccurs="0"/>
            </xs:sequence>
          </xs:complexType>
        </xs:element>
      </xs:sequence>
    </xs:complexType>
    <xs:key name="bookKey">
      <xs:selector xpath=".//book"/>
      <xs:field xpath="@isbn"/>
    </xs:key>
    <xs:keyref name="orderRef" refer="bookKey">
      <xs:selector xpath=".//order"/>
      <xs:field xpath="@isbn"/>
    </xs:keyref>
  </xs:element>
</xs:schema>
"#;

#[test]
fn catalog_valid_instance_validates() {
    let s = Schema::compile_str(CATALOG_XSD).unwrap();
    let ok = r#"<catalog xmlns="urn:catalog">
        <books>
            <book isbn="9780132350884">
                <title>Clean Code</title>
                <categories>programming software-engineering</categories>
                <year>2008</year>
            </book>
            <book isbn="9780201633610">
                <title>Design Patterns</title>
                <categories>programming software-engineering classics</categories>
            </book>
        </books>
        <orders>
            <order who="alice" isbn="9780132350884"/>
            <order who="bob"   isbn="9780201633610"/>
        </orders>
    </catalog>"#;
    s.validate_str(ok).unwrap();
}

#[test]
fn catalog_rejects_bad_isbn_format() {
    let s = Schema::compile_str(CATALOG_XSD).unwrap();
    let bad = r#"<catalog xmlns="urn:catalog">
        <books>
            <book isbn="not-an-isbn">
                <title>X</title>
                <categories>a</categories>
            </book>
        </books>
        <orders/>
    </catalog>"#;
    let err = s.validate_str(bad).unwrap_err();
    assert!(!err.issues.is_empty());
}

#[test]
fn catalog_rejects_too_many_categories() {
    let s = Schema::compile_str(CATALOG_XSD).unwrap();
    let bad = r#"<catalog xmlns="urn:catalog">
        <books>
            <book isbn="9780132350884">
                <title>X</title>
                <categories>a b c d e f g</categories>
            </book>
        </books>
        <orders/>
    </catalog>"#;
    let err = s.validate_str(bad).unwrap_err();
    assert!(err.issues.iter().any(|i| i.message.contains("maxLength")
        && i.message.contains("item(s)")
    ), "expected list-maxLength failure counting items, got {:?}", err.issues);
}

#[test]
fn catalog_rejects_invalid_category_token() {
    let s = Schema::compile_str(CATALOG_XSD).unwrap();
    let bad = r#"<catalog xmlns="urn:catalog">
        <books>
            <book isbn="9780132350884">
                <title>X</title>
                <categories>good BAD</categories>
            </book>
        </books>
        <orders/>
    </catalog>"#;
    let err = s.validate_str(bad).unwrap_err();
    assert!(err.issues.iter().any(|i| i.message.contains("list item")),
        "expected list-item rejection, got {:?}", err.issues);
}

#[test]
fn catalog_rejects_dangling_order_reference() {
    let s = Schema::compile_str(CATALOG_XSD).unwrap();
    let bad = r#"<catalog xmlns="urn:catalog">
        <books>
            <book isbn="9780132350884">
                <title>X</title>
                <categories>a</categories>
            </book>
        </books>
        <orders>
            <order who="alice" isbn="9999999999999"/>
        </orders>
    </catalog>"#;
    let err = s.validate_str(bad).unwrap_err();
    assert!(err.issues.iter().any(|i|
        matches!(i.kind, sup_xml::xsd::ValidationKind::KeyRefDangling)
    ), "expected KeyRefDangling, got {:?}", err.issues);
}

#[test]
fn catalog_rejects_duplicate_book_isbn() {
    let s = Schema::compile_str(CATALOG_XSD).unwrap();
    let bad = r#"<catalog xmlns="urn:catalog">
        <books>
            <book isbn="9780132350884"><title>A</title><categories>x</categories></book>
            <book isbn="9780132350884"><title>B</title><categories>y</categories></book>
        </books>
        <orders/>
    </catalog>"#;
    let err = s.validate_str(bad).unwrap_err();
    assert!(err.issues.iter().any(|i|
        matches!(i.kind, sup_xml::xsd::ValidationKind::KeyNotUnique)
    ), "expected KeyNotUnique, got {:?}", err.issues);
}

// ── audit-uncovered bug: xsi:nil = "1" should be equivalent to "true" ────────
//
// xs:boolean is the type of xsi:nil per XSD §3.4.1, and xs:boolean's
// lexical space is {"true", "false", "1", "0"}.  All four values are
// valid; "1" is value-equal to "true".
//
// The validator's `find_xsi_nil(&attrs) == Some("true")` check at
// crates/core/src/xsd/validate.rs only recognises the literal "true".
// Documents using xsi:nil="1" (less common but spec-compliant) have
// their content validated as if xsi:nil weren't set — which incorrectly
// rejects nillable elements whose content placeholder isn't valid for
// the declared type.
//
// These tests pin the spec-correct behaviour.  They will fail until
// the validator parses xsi:nil as xs:boolean.

#[test]
fn xsi_nil_true_skips_content_validation_on_nillable_element() {
    // Baseline: xsi:nil="true" works today.  Content "not-a-number"
    // would fail xs:int validation, but xsi:nil overrides it.
    let xsd = r#"<?xml version="1.0"?>
<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema">
  <xs:element name="port" type="xs:int" nillable="true"/>
</xs:schema>"#;
    let s = Schema::compile_str(xsd).unwrap();
    let xml = r#"<port xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance"
                       xsi:nil="true">not-a-number</port>"#;
    // The xsi:nil rule also requires the element be empty — content
    // is forbidden.  So this should fail with "xsi:nil element must
    // have empty content", not with a type-mismatch on the int.
    let err = s.validate_str(xml).unwrap_err();
    assert!(err.issues.iter().any(|i| i.message.contains("xsi:nil")),
        "expected xsi:nil error, got {:?}", err.issues);
}

// xsi:nil takes an xs:boolean value (XSD 1.0 §3.2.2).  The lexical
// space is exactly {"true", "false", "1", "0"} — case-sensitive.
// Everything else is a lexical error; the validator emits a
// type-mismatch and treats the element as not-nil for downstream
// validation.

#[test]
fn xsi_nil_one_enables_nil_per_xs_boolean() {
    let xsd = r#"<?xml version="1.0"?>
<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema">
  <xs:element name="port" type="xs:int" nillable="true"/>
</xs:schema>"#;
    let s = Schema::compile_str(xsd).unwrap();
    let xml = r#"<port xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance"
                       xsi:nil="1"></port>"#;
    let result = s.validate_str(xml);
    assert!(result.is_ok(),
        "xsi:nil=\"1\" should be recognised as nil per xs:boolean; got {:?}",
        result.err().map(|e| e.issues));
}

#[test]
fn xsi_nil_true_enables_nil() {
    let xsd = r#"<?xml version="1.0"?>
<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema">
  <xs:element name="port" type="xs:int" nillable="true"/>
</xs:schema>"#;
    let s = Schema::compile_str(xsd).unwrap();
    let xml = r#"<port xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance"
                       xsi:nil="true"></port>"#;
    assert!(s.validate_str(xml).is_ok());
}

#[test]
fn xsi_nil_zero_disables_nil() {
    let xsd = r#"<?xml version="1.0"?>
<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema">
  <xs:element name="port" type="xs:int" nillable="true"/>
</xs:schema>"#;
    let s = Schema::compile_str(xsd).unwrap();
    let xml = r#"<port xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance"
                       xsi:nil="0">not-a-number</port>"#;
    let err = s.validate_str(xml).unwrap_err();
    // Should report an int-validation error, not an xsi:nil error.
    assert!(err.issues.iter().all(|i| !i.message.contains("xsi:nil")),
        "xsi:nil=\"0\" should NOT trigger nil-handling; got {:?}",
        err.issues);
}

#[test]
fn xsi_nil_false_disables_nil() {
    let xsd = r#"<?xml version="1.0"?>
<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema">
  <xs:element name="port" type="xs:int" nillable="true"/>
</xs:schema>"#;
    let s = Schema::compile_str(xsd).unwrap();
    let xml = r#"<port xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance"
                       xsi:nil="false">not-a-number</port>"#;
    let err = s.validate_str(xml).unwrap_err();
    assert!(err.issues.iter().all(|i| !i.message.contains("xsi:nil")),
        "xsi:nil=\"false\" should NOT trigger nil-handling; got {:?}",
        err.issues);
}

#[test]
#[allow(non_snake_case)] // name encodes the literal-case `TRUE` under test
fn xsi_nil_uppercase_TRUE_is_lexical_error() {
    // "TRUE" is NOT in xs:boolean's lexical space — case matters.
    // Per spec the validator must reject this with a type-mismatch
    // pointing at the bad value, not silently accept it.
    let xsd = r#"<?xml version="1.0"?>
<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema">
  <xs:element name="port" type="xs:int" nillable="true"/>
</xs:schema>"#;
    let s = Schema::compile_str(xsd).unwrap();
    let xml = r#"<port xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance"
                       xsi:nil="TRUE"></port>"#;
    let err = s.validate_str(xml).unwrap_err();
    assert!(err.issues.iter().any(|i|
        i.message.contains("xsi:nil") && i.message.contains("xs:boolean")
    ), "expected xsi:nil xs:boolean lexical error, got {:?}", err.issues);
}

#[test]
#[allow(non_snake_case)] // name encodes the literal-case `True` under test
fn xsi_nil_capitalised_True_is_lexical_error() {
    // Same as above for "True" — every non-spec spelling is invalid.
    let xsd = r#"<?xml version="1.0"?>
<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema">
  <xs:element name="port" type="xs:int" nillable="true"/>
</xs:schema>"#;
    let s = Schema::compile_str(xsd).unwrap();
    let xml = r#"<port xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance"
                       xsi:nil="True"></port>"#;
    let err = s.validate_str(xml).unwrap_err();
    assert!(err.issues.iter().any(|i|
        i.message.contains("xsi:nil") && i.message.contains("xs:boolean")
    ), "expected xsi:nil xs:boolean lexical error, got {:?}", err.issues);
}

#[test]
fn xsi_nil_garbage_value_is_lexical_error() {
    // Sanity that the rejection isn't case-bound but applies to any
    // out-of-lexical-space value.
    let xsd = r#"<?xml version="1.0"?>
<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema">
  <xs:element name="port" type="xs:int" nillable="true"/>
</xs:schema>"#;
    let s = Schema::compile_str(xsd).unwrap();
    let xml = r#"<port xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance"
                       xsi:nil="yes"></port>"#;
    let err = s.validate_str(xml).unwrap_err();
    assert!(err.issues.iter().any(|i|
        i.message.contains("xsi:nil") && i.message.contains("xs:boolean")
    ), "expected xsi:nil xs:boolean lexical error, got {:?}", err.issues);
}
