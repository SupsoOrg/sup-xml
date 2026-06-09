# sup-xml

A memory-safe, fast, spec-compliant XML toolkit for Rust — parsing,
serialization, XPath, XSD, XSLT, RelaxNG, Schematron, C14N, and HTML5 in
one library, with a clean idiomatic API and no `unsafe` in your way.

This crate (`sup-xml`) is the idiomatic Rust API. The same engine also
ships as a [drop-in libxml2 ABI replacement](https://supso.org/projects/sup-xml/docs)
for code that links libxml2 today (C/C++, Python `lxml`, Ruby `nokogiri`,
Perl `XML::LibXML`).

- **Memory-safe** — pure Rust with a small, audited `unsafe` core, enforced
  by `#![forbid(unsafe_code)]` everywhere else and exercised under Miri in CI.
- **Spec-compliant** — 255 / 257 on the W3C XML Conformance Test Suite,
  ahead of libxml2's 250 / 257.
- **Fast** — roughly 2× libxml2 and ~1.04× quick-xml on matched-contract
  byte-event throughput.

## Install

```toml
[dependencies]
sup-xml = "1.0"

# Optional features — pull in only what you need:
sup-xml = { version = "1.0", features = ["xsd", "xslt", "html"] }
```

Requires **Rust 1.85+** (`edition = "2024"`).

## Quick start

```rust
use sup_xml::{parse_str, ParseOptions, XPathContext};

let opts = ParseOptions { namespace_aware: true, ..Default::default() };
let doc = parse_str("<catalog><book id='b1'/><book id='b2'/></catalog>", &opts)?;

let ctx = XPathContext::new(&doc);
assert_eq!(ctx.eval_count("/catalog/book")?, 2);
# Ok::<(), sup_xml::Error>(())
```

## Features

| Feature flag | Enables |
|--------------|---------|
| (default) | XML 1.0 parse / serialize, XPath 1.0 |
| `xsd` | XML Schema 1.0 / 1.1 validation (`sup_xml::xsd`) |
| `xslt` | XSLT 1.0 engine + Schematron (`sup_xml::xslt`) |
| `html` | Lenient HTML5 parser (`sup_xml::html`) |
| `serde` | Typed deserialization into Rust values (`sup_xml::de`) |
| `tokio` | Async parse entry points (`sup_xml::async_io`) |
| `network-resolver` | HTTPS-fetched DTDs / entities (`NetworkResolver`) |

## License

SupXML is **source-available** software released through
[Supported Source](https://supso.org/). The source is public and this crate
installs normally, but **a valid license certificate is required to use it** —
without one, document parsing returns a fatal error (a grace period applies
after an existing certificate expires).

| Use | License |
|-----|---------|
| Company, government, or organization | Paid commercial license |
| Evaluating before you decide | Free 30-day evaluation license |
| Individual, non-monetized project | Free, renewable one-year hobbyist license |

Get a certificate at
[supso.org/projects/sup-xml](https://supso.org/projects/sup-xml) and place it
where SupXML looks — `~/.supso/license_certificates/` or a project-local
`./.supso/license_certificates/`. Full terms are in the repository `LICENSE`.

## Documentation

- [Project docs](https://supso.org/projects/sup-xml/docs)
- [API reference (docs.rs)](https://docs.rs/sup-xml)
- [Source on GitHub](https://github.com/SupsoOrg/sup-xml)
