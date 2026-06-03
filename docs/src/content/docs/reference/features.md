---
title: Feature flags
description: Cargo feature flags — what they enable, what they pull in, what's on by default, what each one costs in binary size.
---

```toml
[dependencies]
sup-xml = { version = "*", features = ["xsd", "xslt", "html"] }
```

| Feature | Default? | Pulls in | Enables |
|---|---|---|---|
| `xsd` | off | (nothing extra) | XSD 1.0 / 1.1 validation |
| `xslt` | off | (nothing extra) | XSLT 1.0 + 2.0 transforms (3.0 partial), Schematron |
| `html` | off | (nothing extra) | HTML5 tokenizer and tree builder |
| `serde` | off | `serde` | `from_str` / `from_bytes` for typed structs |
| `tokio` | off | `tokio` | Async I/O entry points |
| `network-resolver` | off | `reqwest`, `ring` | HTTPS-fetched DTDs and entities |
| `full-encodings` | **on** | `encoding_rs` | Full WHATWG encoding set |
| `c-abi` | off (internal) | (changes `tree` layout) | Used by `sup-xml-compat` |

## Binary size

Measured against a representative consumer binary (LTO, codegen-units=1,
`panic=abort`, `strip=true` — the configuration most apps ship). Numbers
are total binary size, not the rlib (the rlib is a thin shim; the
parser, XPath engine, XSD/XSLT modules etc. live in the linked-out
`sup-xml-core` rlib that the linker pulls in on demand).

| Consumer | Features | Binary | Δ vs previous row |
|---|---|---:|---:|
| `parse_str` only | minimal (default-features = false) | **683 KB** | — |
| `parse_str` + `serialize` + `XPathContext` | default | **2.5 MB** | +1.8 MB (XPath engine) |
| above + Schema validate | `xsd` | **2.9 MB** | +0.4 MB |
| above + HTML5 parse | `html` | **3.1 MB** | +0.2 MB |
| above + Stylesheet apply | `xslt` | **3.6 MB** | +0.5 MB |
| above, exercising the full `xsd` + `xslt` + `html` surface | full | **5.2 MB** | +1.6 MB |
| `sup-xml` CLI (all features) | full | **11 MB** | — |
| `libsup_xml_compat.dylib` (drop-in libxml2 ABI) | `c-abi` cdylib | **9.0 MB** | — |

Numbers are macOS arm64; Linux x86_64 sizes are within 5%. Cost per
feature is approximate — the linker dead-code-eliminates anything the
consumer doesn't actually call, so a feature you turn on but never
touch contributes less than the table suggests.

Reproduce with `cargo build --release --features <set>` against a
binary that actually calls the relevant API; see `crates/cli/` for a
worked example with the full surface.

## WASM compatibility

SupXML builds and runs cleanly under `wasm32-unknown-unknown` with the
default feature set, and with any combination of `xsd`, `xslt`, `html`,
`serde`. We test this in CI — the same parser, validator, transform
engine, XPath/XSLT evaluator, and HTML5 tokenizer that ship on native
also run in the browser and in WASM runtimes like Wasmtime, Wasmer, and
Cloudflare Workers.

Concretely:

- **No `std::fs` in the parser core.** Document loading is trait-based
  (`EntityResolver`), so a WASM consumer can plug in their own
  fetch-from-`window.fetch` resolver without touching anything else.
- **No `std::thread` dependency.** Multi-document workloads are
  driven by the consumer, not by SupXML spawning workers.
- **No `std::env` reads** in any hot path. No global-config side-channels.

Only the `network-resolver` feature is unsupported on WASM — it
transitively pulls `ring`, whose C/asm code can't target WASM. If you
need HTTPS-fetched DTDs from inside a WASM module, write a small
`EntityResolver` that delegates to your runtime's HTTP client.

## Picking features

- **Minimal** — `default-features = false` plus what you need. XML 1.0 + XPath only.
- **Standard** — `["xsd", "xslt", "html"]`. Covers most workloads.
- **Full** — `["xsd", "xslt", "html", "serde", "tokio", "network-resolver"]`.

`full-encodings` is on by default because the encoding gap between
the default WHATWG set and `encoding_rs`'s full set is small but
load-bearing for documents declaring `<?xml encoding="EUC-JP"?>`,
`<?xml encoding="ISO-8859-15"?>`, etc. Turn it off only when your
inputs are guaranteed UTF-8 / UTF-16 / ASCII.
