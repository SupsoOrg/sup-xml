---
title: Schematron
description: Rule-based XML validation for the constraints XSD can't express — cross-element rules, conditional requirements, business logic.
---

XSD validates *structure* (what elements appear where, what types
they hold). Schematron validates *rules* (when this attribute is X,
that other element must be Y; book years can't be negative; an order
line's `qty × price` must equal the order's `total`). Use both
together when one alone isn't enough.

## Enabling

The Schematron compiler ships behind the `xslt` feature (Schematron
compiles to XSLT internally):

```toml
[dependencies]
sup-xml = { version = "*", features = ["xslt"] }
```

## Compile and validate

```rust
use sup_xml::xslt::schematron::Schematron;

let sch = Schematron::compile_str(r#"
    <sch:schema xmlns:sch="http://purl.oclc.org/dsdl/schematron">
      <sch:pattern id="book-rules">
        <sch:rule context="book">
          <sch:assert test="@isbn">every book must have an ISBN</sch:assert>
          <sch:assert test="@year &gt;= 1450">books before 1450 predate movable type</sch:assert>
          <sch:report test="@year &lt; 1900">marked as historical</sch:report>
        </sch:rule>
      </sch:pattern>
    </sch:schema>"#)?;

let report = sch.validate_str("<book isbn='978-…' year='1850'/>")?;
for finding in &report.findings {
    println!("[{:?}]  {}  (at <{}>)",
        finding.kind, finding.message, finding.context_name);
}
```

Each `Finding` carries `kind`, `message`, the firing `pattern_id` /
`assertion_id` / `role`, a stable `location_id` for the offending node,
and its `context_name` (the node's local-name).

`Finding::kind` distinguishes `FailedAssert` (an `<assert>` test failed)
from `SuccessfulReport` (a `<report>` test fired its diagnostic), so
consumers can route them to errors vs informational logs.

## How it works

The Schematron schema is compiled to an XSLT stylesheet — this is
exactly what the ISO Schematron reference implementation does — and
run through SupXML's XSLT engine. That means:

- XPath expressions in `<sch:assert>` / `<sch:report>` get the full
  XPath 1.0 + 2.0 surface SupXML ships (axes, predicates, EXSLT
  functions, namespace handling, etc.).
- Performance is whatever XSLT performance is — see the
  [XSLT guide](/guides/xslt/) and the
  [performance reference](/reference/performance/).
- `<sch:phase>`, `<sch:pattern>`, `<sch:let>`, and `<sch:include>` all
  desugar to the corresponding XSLT constructs at compile time.

## Combining with XSD

For documents that need both structural and rule validation, run XSD
first (cheaper, errors more local) and Schematron second (catches
what XSD can't express):

```rust
use sup_xml::xsd::Schema;
use sup_xml::xslt::schematron::Schematron;

let xsd = Schema::compile_str(xsd_source)?;
let sch = Schematron::compile_str(sch_source)?;

xsd.validate_str(instance)?;                       // structure
let report = sch.validate_str(instance)?;          // business rules
if !report.findings.is_empty() {
    eprintln!("{} schematron findings", report.findings.len());
}
```

Typical pattern in regulated domains (financial messaging, ePub,
healthcare data exchange) — the XSD locks the schema shape, the
Schematron enforces the policy.

## From the shell

```bash
sup-xml validate --schematron rules.sch instance.xml
sup-xml validate --schema schema.xsd --schematron rules.sch instance.xml
```

The CLI's `validate` subcommand takes `--schematron` (and can
combine it with `--schema` for XSD + Schematron in one pass). Exit
code is 0 on no findings, 1 if any rule fired.
