---
title: Why SupXML
description: Memory safety, spec compliance, and libxml2 compatibility — the case for replacing the XML processor in your stack.
---

SupXML exists because the XML processing layer in most production systems
is still libxml2, but libxml2 was nearly unmaintained recently, 
and it written in C. SupXML is a memory-safe, spec-compliant, 
libxml2-compatible XML library written in Rust that's 2x as fast
as libxml2, able to serve as a drop-in replacement.

## Safety

SupXML has been designed from day one with safety in mind. That's why it's
built in Rust, for one. SupXML avoids memory-unsafe XML parsing
because the parser is written in Rust, and the `unsafe` surface is a small audited core
where every block has a `SAFETY:` comment with a `Why unsafe:`
justification, under Miri on every PR. We also use fuzzing to check
for any unknown unknowns.

### vs libxml2

libxml2 is written in C. Its CVE history shows memory-unsafe XML
parsing is dangerous. There have been buffer overflows, use-after-free, 
and entity-expansion DoS that are largely the cost of doing XML 
parsing in a language without
bounds-checked arrays. 

### vs quick-xml

`quick-xml` is a pure-Rust XML parser optimized for raw speed.
It does not aim to enforce well-formedness or to be safe for
untrusted input. The trusted/untrusted boundary in real
deployments is rarely as clear as documentation assumes, and a
permissively-parsed XML file from an upstream system can carry
exploits that quick-xml's default tokenizer quietly accepts. For
that reason it isn't a fit for organizations parsing data they
don't fully control. We recommend SupXML instead.

## Drop-in C ABI

The `sup-xml-compat` crate ships a `libsupxml2.so` that's byte-compatible
with libxml2's `_xmlNode`, `_xmlAttr`, `_xmlNs` struct layouts. Existing
C/C++ code that links libxml2 can swap to libsupxml2 by adjusting the
linker. This allows a company to migrate quickly without having
to rewrite code. Full ABI compatibility with `libxml2.so.2` is provided.

## Spec compliance

On the W3C XML Conformance Test Suite (revision `xmlts20130923`), SupXML
passes all **2274 / 2274** tests with a deterministic expected outcome.
21 of the 2295 total tests have catalog outcome `error` 
(implementation-defined per the spec) and are skipped, since the 
correct implementation is left up to the implementation (us)
so it isn't meaningful to test that we precisely match an
ambiguous outcome.

[Full conformance breakdown →](/reference/conformance/)

## Modern build, no global state

libxml2 has process-global initialization, global error callbacks, and a
single shared dictionary. SupXML has none of that. Every `parse_str` call
allocates a fresh arena, errors are returned as `Result`, and there's no
`xmlInitParser` to forget to call.

## Built-in features that are bolt-ons elsewhere

- **HTML5 parsing** (`html` feature) — built-in, no separate library.
- **XSD 1.0 / 1.1 validation** (`xsd`) — exercised against the W3C XSD test suite.
- **XSLT 1.0 + 2.0** (`xslt`) — built on the same XPath engine as the core. EXSLT functions always on. 2.0 is largely done. 3.0 is not implemented yet.
- **Schematron** — compiles to XSLT, runs through the XSLT engine. One pipeline.
- **Canonical XML / Exc-C14N** — for XML-DSig / SAML signing.
- **Async I/O** (`tokio`) — first-class, not a wrapper.
