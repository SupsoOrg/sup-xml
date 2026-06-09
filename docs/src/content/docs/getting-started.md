---
title: Getting started
description: Get going with SupXML in a minute, whether you're writing new code or dropping it in for libxml2.
---

Welcome! SupXML is a modern, memory-safe XML toolkit — parsing,
serialization, XPath, XSD, XSLT, RelaxNG, Schematron, C14N, and HTML5,
all in one library. There are two ways to use it, and you can pick
whichever fits where you're starting from:

- **Writing new Rust?** Reach for the [`sup-xml`](#path-1-the-rust-library)
  crate, a clean, idiomatic API with no `unsafe` in your way.
- **Using another language?** You can still use the Rust code from any
  language with a foreign function interface.
- **Already using libxml2?** SupXML is a
  [drop-in ABI replacement](#path-2-the-libxml2-drop-in). If your code
  (or Python's `lxml`, Ruby's `nokogiri`, Perl's `XML::LibXML`) links
  libxml2 today, you can swap in SupXML's shared library and get
  memory safety **without changing a line of your code**.

All paths share the same engine underneath. Let's get you running.

:::note[A license certificate is required]
SupXML is **source-available** software released through
[Supported Source](https://supso.org/): the source is public and the crates
install normally, but **document parsing requires a valid license
certificate**. Without one, parse entry points return a fatal error (a
short grace period applies after an existing certificate expires).

Grab a certificate — **free** for individuals and non-monetized projects,
free 30-day evaluation for organizations, at
[supso.org/projects/sup-xml](https://supso.org/projects/sup-xml), then drop it
in a folder you create: `~/.supso/license_certificates/` (or a project-local
`./.supso/license_certificates/`). See the
[licensing docs](https://supso.org/projects/sup-xml/docs) for the full model.
:::

---

## Using the Rust library

### Install

Add the crate to your `Cargo.toml`:

```toml
[dependencies]
sup-xml = "1.0"

# Optional features — pull in only what you need:
sup-xml = { version = "1.0", features = ["xsd", "xslt", "html"] }
```

You'll need **Rust 1.85+** (the minimum for `edition = "2024"`, which the
workspace pins). That's all — let's parse something.

### Parse and query

Parse a document and ask it questions with XPath:

```rust
use sup_xml::{parse_str, ParseOptions, XPathContext};

let opts = ParseOptions { namespace_aware: true, ..Default::default() };
let doc = parse_str(
    "<catalog><book id='b1'/><book id='b2'/></catalog>",
    &opts,
)?;

let ctx = XPathContext::new(&doc);
assert_eq!(ctx.eval_count("/catalog/book")?, 2);
```

### Serialize back to XML

```rust
use sup_xml::{parse_str, serialize_to_string};

let doc = parse_str("<r a='1' b='2'/>", &Default::default())?;
let xml = serialize_to_string(&doc);
assert!(xml.contains("<r"));
```

### Validate against XSD

```rust
use sup_xml::xsd::Schema;

let schema = Schema::compile_str(r#"
    <xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema"
               targetNamespace="urn:demo" xmlns="urn:demo">
      <xs:element name="port" type="xs:int"/>
    </xs:schema>"#)?;

schema.validate_str(r#"<port xmlns="urn:demo">8080</port>"#)?;
```

### Transform with XSLT

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
let doc    = parse_str("<catalog><book id='b1'/></catalog>", &opts)?;
let result = style.apply(&doc)?;
println!("{}", result.to_string()?);
```

### From the shell

Prefer the command line? Install the CLI and you have a Swiss-army knife
for XML:

```bash
cargo install sup-xml-cli

sup-xml lint myfile.xml
sup-xml xpath '/catalog/book/@id' input.xml
sup-xml validate --schema schema.xsd instance.xml
sup-xml xslt --stylesheet style.xsl input.xml -o output.xml
sup-xml format --pretty input.xml             # re-emit pretty-printed
sup-xml stats input.xml                       # sizes, depths, counts
sup-xml c14n input.xml                        # Canonical XML / Exc-C14N
```

Run `sup-xml --help` or `sup-xml <command> --help` for the full flag
surface — including `--allow-fs` / `--allow-host` for DTD and entity
fetches, `--xinclude` for `<xi:include>` resolution, and `--html` for
HTML5 input.

---

## Using SupXML as a libxml2 drop-in replacement

Have a codebase that already speaks libxml2 — directly in C/C++, or
through a binding like Python's `lxml`, Ruby's `nokogiri`, or Perl's
`XML::LibXML`? You don't have to rewrite any of it. SupXML ships a
shared library with **byte-compatible struct layouts and the same C
symbols** as libxml2, so you can slot it in underneath your existing
code and get a memory-safe parser for free.

### Build the shared library

```bash
cargo build --release -p sup-xml-compat --features cdylib-exports
```

This produces a libxml2-compatible shared library
(`libsup_xml_compat.so` on Linux, `.dylib` on macOS) exporting
`xmlReadMemory`, `xmlFree`, `xmlNodeGetContent`, and the rest of the
libxml2 surface.

### Swap it in

Put the library on your loader path as `libxml2` (rename or symlink it,
e.g. to `libxml2.so.2`), and switch linker flags from `-lxml2` to point
at it. Your application keeps calling the same libxml2 functions — they
just resolve to SupXML now. The
[Migrating from libxml2](/guides/migrating-from-libxml2/) guide walks
through the exact linker flags, filenames, and the handful of
deliberate behavioural differences.

### Does it really work?

This path is verified end-to-end against the most demanding libxml2
consumers in the wild:

- **Python `lxml`** — 47 / 47 operations behaviourally identical to system libxml2.
- **Ruby `nokogiri`** — 17 / 17 operations identical.
- **Perl `XML::LibXML`** — the widest libxml2 surface of any binding, exercised against the shim.

---

## Where to next

- [Migrating from libxml2](/guides/migrating-from-libxml2/) — the full function-by-function mapping and ABI details.
- [Parsing guide](/guides/parsing/) — options, recovery mode, encodings, security knobs.
- [XPath guide](/guides/xpath/) — XPath 1.0 by default, XPath 2.0+ via the static-context flag.
- [XSD validation guide](/guides/xsd/) — schemas, identity constraints, error reporting.
- [Full Docs.rs API ↗](https://docs.rs/sup-xml)
