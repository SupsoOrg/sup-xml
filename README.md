# SupXML

A memory-safe, fast, spec-compliant XML library written in Rust, with
a drop-in ABI replacement for libxml2.

- **Memory-safe** — pure Rust; the `unsafe` surface is a small,
  audited core, enforced by `#![forbid(unsafe_code)]` on every
  other module and exercised under [Miri](https://github.com/rust-lang/miri)
  in CI.  See [`CONTRIBUTING.md`](CONTRIBUTING.md) § "Unsafe policy".
- **Spec-compliant** — full XML 1.0 well-formedness checks on
  every parse.  Scores **255 / 257** on the W3C XML Conformance
  Test Suite, ahead of libxml2's 250 / 257.  See
  [`COMPARISON.md`](COMPARISON.md) for the side-by-side comparison
  against libxml2, roxmltree, xml-rs, and quick-xml.
- **Fast** — competitive with libxml2 and ~1.04× faster than
  quick-xml on matched-contract head-to-head bytes events.  See
  [`COMPARISON.md`](COMPARISON.md) § "Performance".
- **Recovers gracefully from malformed input** when you want it to
  — opt-in `recovery_mode: true` mode matches libxml2's
  `XML_PARSE_RECOVER` on 12 of 13 common malformed-input scenarios
  and preserves user data in three cases where libxml2 silently
  corrupts the text.  See [`COMPARISON.md`](COMPARISON.md) §
  "Error-recovery mode".

> **License:** SupXML is **source-available**, not open-source. The source
> is public, but *running it requires a license certificate* — parsing
> fails without one. Certificates are free for individuals (hobbyist) and
> for a 30-day company evaluation; commercial use is paid. Get yours at
> [supso.org/projects/sup-xml](https://supso.org/projects/sup-xml). See
> [License](#license) below.

## At a glance

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

```toml
[dependencies]
sup-xml = { version = "*", features = ["xsd", "xslt", "html"] }
```

The CLI bundles every feature: `cargo install --path crates/cli`
gives you a `sup-xml` binary with `lint`, `format`, `xpath`,
`xslt`, `validate`, `repair`, `stats`, `c14n` subcommands.

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

### Validate with Schematron  (feature `xslt`)

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

### Canonicalize for XML-DSig / SAML

```rust
use sup_xml::{parse_str, canonicalize_to_bytes, CanonicalizeOptions, C14nMode};

let doc  = parse_str("<r b='2' a='1'/>", &Default::default())?;
let opts = CanonicalizeOptions { mode: C14nMode::Inclusive_1_0, ..Default::default() };
let c14n = canonicalize_to_bytes(&doc, &opts)?;
```

## Recovering from malformed XML

For trusted-but-buggy input (third-party RSS / Atom feeds, legacy
data migration, diagnostic UIs), opt into recovery mode:

```rust
use sup_xml::{XmlBytesReader, ParseOptions, BytesEvent};

let xml = b"<r>tom & jerry<unclosed>";   // bare & + missing end tag

let opts = ParseOptions { recovery_mode: true, ..Default::default() };
let mut reader = XmlBytesReader::from_bytes(xml).unwrap()
    .with_options(opts);

loop {
    match reader.next().unwrap() {
        BytesEvent::Eof => break,
        _ => {}
    }
}

// Inspect what was wrong — errors are listed in the order they
// were encountered.  Strict-mode callers see these via the first
// returned `Err` instead.
for err in reader.recovered_errors() {
    eprintln!("recovered: {}", err.message);
}
```

The default — `recovery_mode: false` — fails fast on the
first non-trivial error, which is the right behaviour for trusted
internal data.

## Character encodings

The parser auto-detects the input's encoding and transcodes to
UTF-8 before parsing — matching libxml2 and the XML 1.0 spec's
requirement (§ 4.3.3) that processors accept both UTF-8 and
UTF-16:

```rust
use sup_xml::parse_bytes;

let doc = parse_bytes(latin1_or_utf16_or_gb2312_bytes)?;
```

Detection follows XML 1.0 Appendix F — BOM, then the four-byte
autodetect signatures for UTF-32 / UTF-16 / EBCDIC, then the
`<?xml encoding="..."?>` declaration.  UTF-8 input stays
zero-copy; non-UTF-8 input pays one allocation for the decoded
buffer.

To require UTF-8 input — useful when inputs are guaranteed UTF-8
and you want to reject anything else as part of a security
posture — set `auto_transcode: false`:

```rust
use sup_xml::{parse_bytes_opts, ParseOptions};

let opts = ParseOptions { auto_transcode: false, ..Default::default() };
let doc  = parse_bytes_opts(must_be_utf8_bytes, &opts)?;
```

### Supported character encodings

**Built-in, hand-tuned, no external dependency:**

- UTF-8, US-ASCII — zero-copy passthrough
- ISO-8859-1 (Latin-1), Windows-1252 — SWAR-accelerated
- UTF-16 LE / BE — BOM-aware, surrogate-pair validated
- UTF-32 LE / BE — BOM-aware, scalar-validated (also accepts
  `UCS-4LE` / `UCS-4BE` aliases)
- EBCDIC variants — table-driven, byte-for-byte audited:
  - **IBM037** (CCSID 37) — US/Canada Latin, classic mainframe default
  - **IBM500** (CCSID 500) — International EBCDIC (IBM037 +
    rearranged `[`, `]`, `!`, `^`, `|`, `¬`, `¢`)
  - **IBM1047** (CCSID 1047) — Open Systems / z/OS Unix Services
    Latin-1 (IBM500 + LF/NEL line-ending swap)
  - **IBM1140** (CCSID 1140) — IBM037 with the Euro sign update
    (byte 0x9F → `€`)

**Via [`encoding_rs`](https://crates.io/crates/encoding_rs), default
`full-encodings` feature:**

The full WHATWG Encoding set — Shift_JIS, EUC-JP, ISO-2022-JP,
GB2312, GBK, GB18030, Big5, EUC-KR, ISO-8859-2…16,
Windows-1250…1258, KOI8-R, KOI8-U, IBM866, macintosh,
x-mac-cyrillic, TIS-620 (via `windows-874`), and others.  Disable
the feature to drop the dependency and accept a clean error on
these inputs.

### UTF-7 is a deliberately unsupported character encoding

- **UTF-7** — for security reasons, UTF-7 is not supported

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
