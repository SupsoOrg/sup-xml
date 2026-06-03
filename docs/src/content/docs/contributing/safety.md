---
title: Safety policy
description: The audited-unsafe-core model — where unsafe is allowed, the SAFETY contract every block carries, and how we verify it.
---

SupXML is positioned as a memory-safe XML library — a Rust replacement
for libxml2 whose CVE history is full of buffer-overflow and
use-after-free bugs. Most of the workspace is `unsafe`-free; the
small audited core that does need `unsafe` carries a documented
contract on every block, runs under Miri in CI, and is exercised by
a continuous fuzz harness. This page describes the policy in detail.

## Default: no unsafe

Most modules are marked with `#![forbid(unsafe_code)]`. This is a hard
compile-time error — `unsafe { ... }` blocks in those files won't even
build.

## Where unsafe is allowed

The audited unsafe core lives in these files. Each carries `unsafe`
inside a documented contract; the rest of the workspace is marked
`#![forbid(unsafe_code)]` so adding `unsafe` to a new module is a
compile-time error until the attribute is removed in a PR.

Parser pipeline (`crates/core/src/`):

- `scanner.rs` — raw cursor over byte streams
- `reader.rs` — `&str` wrapper over the bytes reader
- `xml_bytes_reader.rs` — pull-style byte reader over Scanner
- `streaming_reader.rs` — rolling-window streaming variant
- `stream_parser.rs` — streaming DOM-build adapter
- `parser.rs` — DOM-build entry points
- `encoding.rs` — UTF-8 / non-UTF-8 boundary

HTML5 sink (`crates/core/src/html/`):

- `mod.rs` — html5ever sink + Document conversion
- `sink.rs` — `TreeSink` implementation hot path

Arena DOM (`crates/tree/src/`):

- `arena.rs` — bumpalo-backed node allocator
- `dom.rs` — owned-vs-borrowed node materialisation

The C ABI shim (`crates/compat/`) is a separate audit: every public
function is `unsafe extern "C"` by FFI requirement (it's the
`libsupxml2.so` drop-in replacement for libxml2). The contract there
is "match libxml2's pointer / lifetime / mutation semantics
byte-for-byte"; the audit is the lxml + nokogiri behavioural test
matrix in `tests/abi-system/`. Don't treat compat as a model for
"how much `unsafe` is acceptable" — it's a deliberate FFI boundary,
not application code.

## The contract

Every `unsafe { ... }` block must be preceded by a comment with **both** of
these two parts:

1. **`SAFETY:`** — exactly *why* the operation is safe.
2. **`Why unsafe:`** — exactly *why* we're not using the safe equivalent.

Example:

```rust
// SAFETY: loop condition `p < end` and `end == bytes.len()`
// together prove `p` is in bounds.
// Why unsafe: this loop runs on every top-level event; safe
// `bytes[p]` adds a per-iteration bounds check that LLVM doesn't
// always elide across the surrounding match arms.
while p < end {
    let b = unsafe { *bytes.get_unchecked(p) };
    ...
}
```

The `Why unsafe:` clause is what stops `unsafe` from spreading. If you
can't write a real perf justification, use safe Rust.

## Verification

- **Miri** in CI on every PR
- **Fuzz harness** in `crates/core/fuzz` (libFuzzer-based)
