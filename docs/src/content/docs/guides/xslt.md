---
title: XSLT
description: Compile XSLT stylesheets and transform documents. XSLT 1.0 and 2.0; substantial 3.0 coverage.
---

## Versions

SupXML ships XSLT 1.0 fully and XSLT 2.0 at **~96.1% conformance on
the W3C XSLT 3.0 test suite** (4510 / 4692 attempted 2.0+ cases). The
engine selects the version from the stylesheet's `version=` attribute
— `version="2.0"` opts in to the 2.0 instruction set (`xsl:function`,
`xsl:analyze-string`, `xsl:for-each-group`, `xsl:perform-sort`,
`xsl:next-match`, `xsl:try` / `xsl:catch`, sequence types,
`as=` typing, etc.), and the XPath 2.0 expression layer is enabled
automatically inside it.

Substantial XSLT 3.0 surface is implemented too: maps, arrays, higher-
order functions, `xsl:iterate`, `xsl:merge`, `xsl:accumulator`,
`xsl:mode`, `xsl:evaluate`, `xsl:source-document`, structured
`err:code` / `err:module` reflection on caught errors, partial
`xsl:package` + `xsl:use-package` linking, JSON (`fn:parse-json`,
`fn:xml-to-json`, `fn:json-to-xml`, `fn:json-doc`). Things that
require XSD 1.1-style schema integration (true `xs:assertion` /
conditional type assignment driven from XPath assertions, streamable
analysis) are not implemented; the engine produces correct non-
streamed output for streamable stylesheets so conforming stylesheets
behave identically.

## Compile and apply

```rust
use sup_xml::{parse_str, ParseOptions};
use sup_xml::xslt::Stylesheet;

let xsl = r#"<xsl:stylesheet version="1.0"
    xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
  <xsl:template match="/catalog">
    <ul><xsl:for-each select="book"><li id="{@id}"/></xsl:for-each></ul>
  </xsl:template>
</xsl:stylesheet>"#;

let style = Stylesheet::compile_str(xsl)?;
let doc = parse_str("<catalog><book id='b1'/></catalog>",
    &ParseOptions { namespace_aware: true, ..Default::default() })?;
let result = style.apply(&doc)?;

println!("{}", result.to_string()?);
```

## EXSLT

All EXSLT functions (math, date, str, set) are available without registration.

## Schematron

Schematron compiles to XSLT and runs through the same engine:

```rust
use sup_xml::xslt::schematron::Schematron;

let sch = Schematron::compile_str(r#"
    <sch:schema xmlns:sch="http://purl.oclc.org/dsdl/schematron">
      <sch:pattern>
        <sch:rule context="book">
          <sch:assert test="@isbn">every book must have an ISBN</sch:assert>
        </sch:rule>
      </sch:pattern>
    </sch:schema>"#)?;

let report = sch.validate_str("<book/>")?;
assert!(!report.findings.is_empty());
```
