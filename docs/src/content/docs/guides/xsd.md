---
title: XSD validation
description: Compile XML Schemas and validate instance documents — XSD 1.0 and 1.1, with DFA-compiled content models and structured error reporting.
---

## Compile a schema

```rust
use sup_xml::xsd::Schema;

let schema = Schema::compile_str(r#"
    <xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema"
               targetNamespace="urn:demo" xmlns="urn:demo">
      <xs:element name="port" type="xs:int"/>
    </xs:schema>"#)?;
```

For schemas that span multiple files (`xs:include`, `xs:import`,
`xs:redefine`), use `Schema::compile_with` and supply a resolver
that knows where to find sibling schemas:

```rust
use sup_xml::xsd::{Schema, FsResolver};

let resolver = FsResolver::new("/srv/schemas");
let schema = Schema::compile_with(xsd_source, resolver)?;
```

The compile step builds a deterministic finite automaton for every
content model, so validation runs linear in input size with no
backtracking — a one-time cost paid at compile and amortised across
every subsequent `validate_*` call.

## Validate

```rust
schema.validate_str(r#"<port xmlns="urn:demo">8080</port>"#)?;

// Or against a parsed document — reuse the parse if you'll query / transform
let doc = sup_xml::parse_str("<port xmlns='urn:demo'>8080</port>", &Default::default())?;
schema.validate_doc(&doc)?;

// Or against a byte slice (encoding auto-detected like `parse_bytes`)
schema.validate_bytes(include_bytes!("instance.xml"))?;
```

## XSD versions

SupXML defaults to **XSD 1.0** because most XSD-using pipelines target
1.0 and silently accepting 1.1 syntax in a 1.0 context is the class
of bug we don't want to ship. Three ways to opt in to 1.1:

```rust
use sup_xml::xsd::{Schema, SchemaOptions, SchemaVersion};

// Explicit 1.1 — every schema is treated as 1.1
let schema = Schema::compile_str_with_options(xsd,
    SchemaOptions { version: SchemaVersion::Xsd11, ..Default::default() })?;

// Auto — start in 1.0, promote to 1.1 when the schema carries
// `vc:minVersion="1.1"` on <xs:schema>.  Good for mixed corpora.
let schema = Schema::compile_str_with_options(xsd,
    SchemaOptions { version: SchemaVersion::Auto, ..Default::default() })?;

// Strict 1.0 (the default) — any 1.1 construct produces a clean
// error pointing at the SchemaOptions setting needed to accept it.
let schema = Schema::compile_str(xsd)?;
```

### XSD 1.1 features SupXML ships

- `xs:dateTimeStamp`, `xs:dayTimeDuration`, `xs:yearMonthDuration`,
  `xs:anyAtomicType`, `xs:error` built-ins
- `xs:explicitTimezone` facet
- `notQName` / `notNamespace` on wildcards (literal QNames, `##local`,
  `##targetNamespace`, `##defined`, `##definedSibling`)
- `xs:override` directive
- `inheritable="true"` on `xs:attribute`
- `vc:minVersion` auto-promotion (via `SchemaVersion::Auto`)

### Not yet shipped in 1.1 mode

- `xs:assert` / `xs:assertion` — needs XPath 2.0 integration with
  PSVI-style type access
- `xs:alternative` (conditional type assignment)
- `xs:openContent` / `xs:defaultOpenContent`

## Error reporting

```rust
match schema.validate_str(bad_xml) {
    Ok(()) => println!("valid"),
    Err(report) => {
        for issue in &report.issues {
            eprintln!("{}:{}: [{}] {}",
                issue.line, issue.column, issue.kind, issue.message);
        }
    }
}
```

`ValidationKind` distinguishes the failure cause — `TypeMismatch`,
`UnexpectedElement`, `MissingRequiredElement`, `MissingRequiredAttribute`,
`KeyNotUnique`, `KeyRefDangling`, `FacetViolation`, etc. — so consumers
can route different failure classes to different surfaces (HTTP 422 vs
500, schema-author warning vs end-user message, etc.).

## From the shell

```bash
sup-xml validate --schema schema.xsd instance.xml
sup-xml validate --schema schema.xsd *.xml          # batch
sup-xml validate --schema schema.xsd --verbose *.xml   # per-file OK lines
```

## Performance

SupXML is **98.9 % conformant on W3C XSTS schemaTest, 98.8 % on
instanceTest, ~2.4× faster than libxml2 wall-clock** on the same
corpus. Full bench breakdown:
[performance reference](/reference/performance/#xsd-validation--head-to-head-conformance--wall-clock).
