# SupXML

A memory-safe, fast, spec-compliant XML library written in Rust, with
a drop-in ABI replacement for libxml2.

- **Memory-safe** SupXML is pure Rust where the `unsafe` surface is a small,
  audited core, enforced by `#![forbid(unsafe_code)]` on every
  other module and exercised under [Miri](https://github.com/rust-lang/miri)
  in CI.  See [`CONTRIBUTING.md`](CONTRIBUTING.md) § "Unsafe policy".
- **Spec-compliant** SupXML scores **255 / 257** on the W3C XML Conformance
  Test Suite, ahead of libxml2's 250 / 257.  See
  [`COMPARISON.md`](COMPARISON.md) for the side-by-side comparison
  against libxml2, roxmltree, xml-rs, and quick-xml.
- **Fast** SupXML is ~2x as fast as libxml2 and ~1.04× faster than
  quick-xml on matched-contract head-to-head bytes events.  See
  [`COMPARISON.md`](COMPARISON.md) § "Performance".
- **Recovers gracefully from malformed input** when you want it to
  with an optional `recovery_mode: true` mode, just like libxml2's
  `XML_PARSE_RECOVER`.  See [`COMPARISON.md`](COMPARISON.md) for more info.
- **Drop-in replacement** for libxml2 with a C ABI shim.

## Documentation

- To view the main documentation page, visit
  [supso.org/projects/sup-xml/docs](https://supso.org/projects/sup-xml/docs).
- To read the programming-level documentation, visit
  [docs.rs/sup_xml](https://docs.rs/sup_xml/latest/sup_xml/).
- [`COMPARISON.md`](COMPARISON.md) — feature, compliance, and
  performance comparison vs other XML parsers
- [`CONTRIBUTING.md`](CONTRIBUTING.md) — code policy, unsafe rules,
  Miri instructions

## License

SupXML is **source-available** software, released through
[Supported Source](https://supso.org/). The source is public on GitHub,
but **a license certificate is required to use it** — without a valid
certificate, document parsing returns a fatal error.

| Use | License |
|-----|---------|
| Company, government, or organization | Paid commercial license |
| Evaluating before you decide | Free 30-day evaluation license |
| Individual, non-monetized project | Free, renewable one-year hobbyist license |

Get a certificate at
[supso.org/projects/sup-xml](https://supso.org/projects/sup-xml) and place
it where SupXML looks for it (`SUPSO_LICENSE`, `~/.supso/license_certificates/`,
or `./.supso/license_certificates/`). Full terms are in [`LICENSE`](LICENSE);
the model is explained in the [licensing docs](https://supso.org/projects/sup-xml/docs)
and the [Supported Source FAQ](https://supso.org/faq).

## Feature table

| Feature | Cargo feature | Entry point |
|---------|---------------|-------------|
| XML 1.0 parse / serialize | (default) | [`parse_str`], [`parse_bytes`], [`serialize_to_string`] |
| XPath 1.0 | (default) | [`XPathContext`], [`xpath_str`], [`xpath_count`] |
| HTML5 parse | `html` | [`parse_html_str`] |
| XSD 1.0 / 1.1 validation | `xsd` | `sup_xml::xsd::Schema` |
| XSLT 1.0 transforms | `xslt` | `sup_xml::xslt::Stylesheet` |
| Schematron validation | `xslt` | `sup_xml::xslt::schematron::Schematron` |
| Canonical XML / Exc-C14N | (default) | [`canonicalize_to_bytes`] |
| Typed-struct deserialize | `serde` | `sup_xml::de::*` |
| HTTPS-fetched DTDs / entities | `network-resolver` | [`NetworkResolver`] |
| Async I/O entry points (tokio) | `tokio` | `sup_xml::async_io::parse_async` |

## Quick start

### Parse and query with XPath

```rust
use sup_xml::{parse_str, ParseOptions, XPathContext};

let opts = ParseOptions { namespace_aware: true, ..Default::default() };
let doc  = parse_str("<catalog><book id='b1'/><book id='b2'/></catalog>", &opts)?;

let ctx = XPathContext::new(&doc);
let n   = ctx.eval_count("count(/catalog/book)")?;
assert_eq!(n, 2);
```

### Transform with XSLT 1.0  (feature `xslt`)

```rust
use sup_xml::{parse_str, ParseOptions};
use sup_xml::xslt::Stylesheet;

let xsl = r#"<xsl:stylesheet version="1.0"
    xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
  <xsl:template match="/catalog">
    <ul><xsl:for-each select="book"><li id="{@id}"/></xsl:for-each></ul>
  </xsl:template>
</xsl:stylesheet>"#;

let style  = Stylesheet::compile_str(xsl)?;
let opts   = ParseOptions { namespace_aware: true, ..Default::default() };
let doc    = parse_str("<catalog><book id='b1'/><book id='b2'/></catalog>", &opts)?;
let result = style.apply(&doc)?;
println!("{}", result.to_string()?);
```

From the shell:

```bash
sup-xml xslt --stylesheet style.xsl input.xml -o output.xml
```

### Validate with XML Schema  (feature `xsd`)

```rust
use sup_xml::xsd::Schema;

let schema = Schema::compile_str(r#"
    <xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema"
               targetNamespace="urn:demo" xmlns="urn:demo">
      <xs:element name="port" type="xs:int"/>
    </xs:schema>"#)?;

schema.validate_str(r#"<port xmlns="urn:demo">8080</port>"#)?;
```

From the shell:

```bash
sup-xml validate --schema schema.xsd instance.xml
```

## Guides

The examples above cover the common cases. Each capability has a full
guide on [supso.org](https://supso.org/projects/sup-xml/docs) covering
the options, edge cases, and reference detail:

- [Parsing & serialization](https://supso.org/projects/sup-xml/docs/guides/parsing/)
- [XPath 1.0](https://supso.org/projects/sup-xml/docs/guides/xpath/)
- [XSLT 1.0 transforms](https://supso.org/projects/sup-xml/docs/guides/xslt/)
- [XSD validation](https://supso.org/projects/sup-xml/docs/guides/xsd/)
- [Schematron](https://supso.org/projects/sup-xml/docs/guides/schematron/) — rule-based validation for the constraints XSD can't express
- [Canonical XML / Exc-C14N](https://supso.org/projects/sup-xml/docs/guides/canonical/) — for XML-DSig, SAML, eIDAS / XAdES, WS-Security
- [Recovery mode](https://supso.org/projects/sup-xml/docs/guides/recovery/) — parsing malformed feeds and legacy data without losing content
- [Character encodings](https://supso.org/projects/sup-xml/docs/guides/encodings/) — auto-detection plus UTF-8/16/32, Latin-1, EBCDIC, and the full WHATWG set
- [HTML5 parsing](https://supso.org/projects/sup-xml/docs/guides/html/)
- [Typed-struct deserialize (serde)](https://supso.org/projects/sup-xml/docs/guides/serde/)
- [Async I/O (tokio)](https://supso.org/projects/sup-xml/docs/guides/async/)
- [Migrating from libxml2](https://supso.org/projects/sup-xml/docs/guides/migrating-from-libxml2/)

## Requirements

- [Rust](https://www.rust-lang.org/tools/install) 1.85+ (edition 2024).
- For Miri / nightly tooling, use [rustup](https://rustup.rs).

### WASM compatibility

Builds cleanly under `wasm32-unknown-unknown` with the default
feature set and with `xslt` / `xsd` / `html` enabled (any
combination).  Only `network-resolver` is unsupported on WASM —
it transitively pulls `ring`, whose C crypto code can't be
compiled for the WASM target.  XML parsing, XPath, XSLT
transforms, XSD validation, and HTML parsing all run in the
browser / on serverless edge runtimes.

## Build / test

```bash
cargo build --release
cargo test --workspace
```

## Workspace layout

```
crates/
  api/      # public re-exports — depend on this from your code
  core/     # the parser, validator, XPath, XSD engine
  tree/     # in-memory document model (Node, ElementNode, …)
  xslt/     # XSLT 1.0 transformation engine (built on core's XPath)
  cli/      # `sup-xml` command-line tool (lint, print, xpath, validate, …)
  bench/    # head-to-head benches vs libxml2, roxmltree, xml-rs, quick-xml
  compat/   # libxml2 C-ABI compatibility shim
```
