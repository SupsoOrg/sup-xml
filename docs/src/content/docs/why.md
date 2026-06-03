---
title: Why SupXML
description: Memory safety, spec compliance, and libxml2 compatibility — the case for replacing the XML processor in your stack.
---

SupXML exists because the XML processing layer in most production systems
is still libxml2 — and libxml2's CVE history is full of buffer overflows,
use-after-free bugs, and entity-expansion DoS. The closest pure-Rust
alternative, `quick-xml`, is fast but offers configurable shortcuts around
well-formedness (you can disable end-tag matching, attribute syntax
validation, and name validation in the name of speed) and uses `unsafe` in
hot paths. We wanted a library that matches libxml2's feature coverage and
ABI but doesn't carry either liability.

## Safety

SupXML has been designed from day one with safety in mind. That's why it's
built in Rust, for example. SupXML avoids memory-unsafe XML parsing
because the parser is written in
Rust, and the `unsafe` surface is a small audited core
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

`quick-xml` is a pure-Rust XML parser optimised for raw speed.
It does not aim to enforce well-formedness or to be safe for
untrusted input — the trusted/untrusted boundary in real
deployments is rarely as clear as documentation assumes, and a
permissively-parsed XML file from an upstream system can carry
exploits that quick-xml's default tokeniser quietly accepts. For
that reason it isn't a fit for organisations parsing data they
don't fully control.

Two structural concerns relative to SupXML:

- **Well-formedness has user-facing escape hatches.** `quick-xml`'s reader
  ships flags like `check_end_names`, `trim_text`, and others that let
  callers disable structural checks for speed. SupXML's default is strict
  well-formedness on every parse, with an explicit opt-in `recovery_mode`
  for lenient parsing (and even then errors are surfaced via
  `recovered_errors()`, not swallowed silently). Different design point:
  `quick-xml` trusts the caller to know when to relax; SupXML defaults to
  spec-correct and forces the caller to opt out.
- **Unsafe in hot paths.** `quick-xml` uses `unsafe` blocks for performance
  in byte-iteration code paths. That's a defensible tradeoff for some
  workloads, but it's the same shape of risk that gives libxml2 its CVE
  history. SupXML's audited-unsafe-core model holds `unsafe` to five
  reviewed files with explicit safety contracts, exercised under Miri.

### What "safe" means concretely in SupXML

- The audited unsafe core is a handful of files in the parser
  pipeline, the HTML5 sink, and the arena DOM (see the
  [safety policy](/contributing/safety/) for the full list). Every
  other module is marked `#![forbid(unsafe_code)]` — a hard
  compile-time error, not a guideline.
- Every remaining `unsafe` block has a `SAFETY:` invariant explanation
  *and* a `Why unsafe:` perf justification.
- CI runs the full test suite under [Miri](https://github.com/rust-lang/miri)
  on every PR. Miri detects out-of-bounds reads/writes, use-after-free,
  aliasing violations, and uninit reads at the MIR-instruction level.
- A fuzz harness (`crates/core/fuzz`) runs continuously against the
  parser entry points.

## Spec compliance

On the W3C XML Conformance Test Suite (revision `xmlts20130923`), SupXML
passes **2274 / 2274** tests with a deterministic expected outcome — zero
failures, zero known-failing entries on the build's allowlist. 21 of the
2295 total tests have catalog outcome `error` (implementation-defined per
the spec) and are skipped.

[Full conformance breakdown →](/reference/conformance/)

## Recovery mode that preserves data

libxml2's `XML_PARSE_RECOVER` mode silently corrupts text in scenarios
where the input contains a bare `&` in PCDATA — the surrounding text is
dropped. SupXML's recovery mode preserves the bare `&` as a literal in the
text node (test-backed in `crates/api/tests/recovery.rs`) and surfaces the
error via `recovered_errors()`.

[Recovery mode details →](/guides/recovery/)

## Drop-in C ABI

The `sup-xml-compat` crate ships a `libsupxml2.so` that's byte-compatible
with libxml2's `_xmlNode`, `_xmlAttr`, `_xmlNs` struct layouts. Existing
C/C++ code that links libxml2 can swap to libsupxml2 by adjusting the
linker. Full ABI compatibility with `libxml2.so.2` is provided.

## Modern build, no global state

libxml2 has process-global initialization, global error callbacks, and a
single shared dictionary. SupXML has none of that. Every `parse_str` call
allocates a fresh arena, errors are returned as `Result`, and there's no
`xmlInitParser` to forget to call.

## Built-in features that are bolt-ons elsewhere

- **HTML5 parsing** (`html` feature) — built-in, no separate library.
- **XSD 1.0 / 1.1 validation** (`xsd`) — exercised against the W3C XSD test suite.
- **XSLT 1.0 + 2.0** (`xslt`) — built on the same XPath engine as the core; EXSLT functions always on. 2.0 is at ~96.1% W3C conformance on the attempted 2.0+ cases.
- **Schematron** — compiles to XSLT, runs through the XSLT engine. One pipeline.
- **Canonical XML / Exc-C14N** — for XML-DSig / SAML signing.
- **Async I/O** (`tokio`) — first-class, not a wrapper.
