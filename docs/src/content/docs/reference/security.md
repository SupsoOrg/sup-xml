---
title: Security model
description: Entity budgets, external entity policy, depth limits, and how SupXML defends against XXE, billion laughs, and bombs.
---

## Defaults

SupXML's defaults are safe for parsing untrusted input. You don't need to
opt into protection, instead you must actively opt out of it.


## Rust is memory-safe

Rust's core design ensures that common memory errors like use-after-free,
double-free, and data races are caught at compile time. This eliminates
entire classes of vulnerabilities that have historically plagued
XML parsers written in unsafe languages like C/C++.

According to [The Urgent Need for Memory Safety in Software Products](https://www.cisa.gov/news-events/news/urgent-need-memory-safety-software-products), 70% of CVEs in
software today are due to memory safety issues.

As one example, libxml2 has shipped 70+ CVEs over its lifetime,
largely buffer overflows, use-after-free bugs, and entity-expansion DoS,
most of which would be
compile-time impossible in SupXML's audited-unsafe model.


## Common XML parsing attacks

Here's a non-exhaustive list of common XML parsing attacks and how SupXML's defaults defend against them:

| Attack | Default defense |
|---|---|
| Billion laughs (entity expansion) | `max_entity_expansion_bytes: 1_000_000` (1 MB) |
| Quadratic blowup | covered by the entity-expansion cap |
| XML external entity (XXE) | `external_resolver: None` (no loads possible) |
| External DTD load | `load_external_dtd: false` |
| Network access during parse | requires explicit `NetworkResolver` |
| Deep nesting (stack overflow) | `max_element_depth: 256` |
| Decompression bomb (gzipped DTD) | external DTD disabled by default |
| XPath complexity blowup (`//*[//*[…]]`) | eval step budget, `DEFAULT_MAX_EVAL_STEPS: 20_000_000` |


## Entity-expansion cap

`max_entity_expansion_bytes` caps the total bytes produced by entity
expansion across a single document. Billion-laughs payloads
(`<!ENTITY x "..."><!ENTITY y "&x;&x;&x;...">` × N) hit the cap and abort
before consuming memory:

```rust
use sup_xml::ParseOptions;

let opts = ParseOptions {
    max_entity_expansion_bytes: 1_000_000,   // 1 MB, the default
    ..Default::default()
};
```

To opt out (e.g., for trusted internal data with large entity expansions):

```rust
let opts = ParseOptions {
    max_entity_expansion_bytes: u64::MAX,
    ..Default::default()
};
```

## External entities

Default-off. Setting an `external_resolver` is the explicit opt-in. The
presence of a resolver IS the permission. There's no global hook to forget.

```rust
use std::sync::Arc;
use std::path::PathBuf;
use sup_xml::{ParseOptions, FilesystemResolver};

let resolver = FilesystemResolver::new(vec![
    PathBuf::from("/srv/schemas"),
    PathBuf::from("/srv/dtds"),
]);
let opts = ParseOptions {
    external_resolver: Some(Arc::new(resolver)),
    ..Default::default()
};
```

The constructor takes a `Vec<PathBuf>` of allowed roots — entity
references are resolved against the URL path and rejected if they
escape every root. No wildcards; no `..` traversal; no symlink
follow.

## Network resolver

For documents that reference HTTPS-hosted DTDs (e.g. DocBook), the
`network-resolver` feature provides `NetworkResolver`. The host
allowlist is **mandatory at construction**: there is no `allow_all()`
or `.allow_host()` builder, by design.

```rust
use std::time::Duration;
use sup_xml::NetworkResolver;

let resolver = NetworkResolver::new(["docbook.org".to_string()])
    .with_timeout(Duration::from_secs(5))
    .with_max_response_bytes(2 * 1024 * 1024);
```

Defaults are: HTTPS-only (no `http://`), 10 s timeout, 1 MiB response
cap, private-IP / loopback / link-local addresses blocked (SSRF
defence). Each of those is configurable via a `with_*` builder, and
each relaxation is named so a code reviewer sees the security cost
in-line.

## Depth limit

```rust
// The default is 256; tighten it for untrusted input:
let opts = ParseOptions { max_element_depth: 64, ..Default::default() };
```

Defends against stack-overflow attacks via deeply-nested XML (`<a><a><a>...`).

## XPath evaluation budget

XPath 1.0 semantics make some expressions super-linear: deeply nested
predicates over the descendant axis (`//*[//*[//*[. = 'x']]]`) cost
O(Nᵏ) in document size N and nesting depth k. A short crafted expression
can otherwise spin for a long time.

Every evaluation is bounded by a **step budget**. When it's exceeded the
evaluation aborts with an error (it never hangs). The default is
`DEFAULT_MAX_EVAL_STEPS` (20,000,000) — comfortable for ordinary and
generated XPath, while catching the adversarial shapes in well under a
second on release builds.

If you evaluate **untrusted XPath**, tighten the ceiling via
`XPathOptions::max_eval_steps` to bound worst-case CPU. The cap applies
to each top-level `eval`, so one reusable context enforces it on every
expression:

```rust
use sup_xml::{parse_str, ParseOptions, XPathContext, XPathOptions};

let doc = parse_str("<r/>", &ParseOptions::default()).unwrap();

// ~0.5s ceiling for untrusted expressions:
let opts = XPathOptions { max_eval_steps: 1_000_000, ..Default::default() };
let ctx = XPathContext::new_with(&doc, opts);
let result = ctx.eval(untrusted_xpath); // Err if it exceeds the budget
```

Raise it instead for trusted, legitimately-expensive generated XPath.
XPath authored by your own code needs no change — the default already
covers it.

## Threat model — what we defend

- Parsing untrusted XML from network sources
- Validating untrusted documents against trusted schemas
- Applying untrusted XSLT (with `network: false`, default)

## Threat model — what we don't defend

- Compiling untrusted XSLT. XSLT is Turing-complete and we don't sandbox
  it. If you must compile attacker-controlled stylesheets, run them in a
  separate process / WASM module with resource limits.
- Compiling untrusted XSDs at unbounded cost. XSD compilation is bounded
  but a 100 MB schema document will take 100 MB to compile.
- Network requests from untrusted documents. The network resolver is opt-in but use it at your own risk.

If you have a specific threat model in mind, [open an issue](https://github.com/SupsoOrg/sup-xml/issues)
and we'll document the answer here.
