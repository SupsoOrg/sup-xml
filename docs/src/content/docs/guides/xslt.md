---
title: XSLT
description: Compile XSLT stylesheets and transform documents. XSLT 1.0 and 2.0; substantial 3.0 coverage.
---

## Versions

SupXML implements XSLT 1.0 in full, XSLT 2.0 broadly, and a substantial
slice of XSLT 3.0. Measured on the W3C XSLT 3.0 test suite (with the
[`xsd` feature](#schema-aware-processing) enabled):

- **XSLT 2.0 — ~95%** (4680 / 4921 of the suite's 2.0+ cases).
- **XSLT 3.0 — ~85%** attempting *every* 2.0+/3.0 case, including the
  streaming and schema-aware families that are only partially
  implemented; **~90%** over the feature surface that is fully
  supported.

The engine selects the version from the stylesheet's `version=`
attribute. `version="2.0"` opts in to the 2.0 instruction set
(`xsl:function`, `xsl:analyze-string`, `xsl:for-each-group`,
`xsl:perform-sort`, `xsl:next-match`, sequence types, `as=` typing,
etc.) and enables the XPath 2.0 expression layer automatically.
`version="3.0"` additionally enables the 3.0 surface below.

### XSLT 3.0 surface

Implemented: maps and arrays, higher-order and inline functions,
`xsl:iterate`, `xsl:merge`, `xsl:accumulator`, `xsl:mode`,
`xsl:evaluate` (including inside `xsl:function` bodies),
`xsl:source-document`, `xsl:try` / `xsl:catch` with structured
`err:code` / `err:module` reflection on caught errors,
`xsl:for-each-group` (group-by / -adjacent / -starting-with /
-ending-with, with `composite` keys), `xsl:on-empty` /
`xsl:on-non-empty` / `xsl:where-populated`, `inherit-namespaces`, text
value templates (`expand-text`), and JSON (`fn:parse-json`,
`fn:xml-to-json`, `fn:json-to-xml`, `fn:json-doc`).

Partial: `xsl:package` / `xsl:use-package` linking, schema-aware
processing, and streaming (see below).

### Schema-aware processing

`xsl:import-schema`, typed pattern matching (`element(*, T)` /
`attribute(*, T)` / `schema-element`), and `cast` / `castable` /
`instance of` against user-defined schema types require the **`xsd`
Cargo feature** — the schema-aware code is compiled out by default:

```toml
sup-xml = { version = "1.3", features = ["xslt", "xsd"] }
```

Full PSVI validation (`validation="strict"` / `"lax"`) and XSD 1.1-style
conditional type assignment are not yet implemented.

### Streaming

`streamable="yes"` stylesheets compile and run, and the §19 posture/
sweep analysis is wired in, but the engine does not stream incrementally
end to end — it produces the correct result by evaluating against the
fully-built tree (in bursts where possible). Conforming streamable
stylesheets therefore behave identically, and
`system-property('xsl:supports-streaming')` reports `no`.

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
