# Contributing to SupXML

Thanks for your interest in contributing.  This document covers the
project's coding standards and the policies that aren't enforced by
the compiler.

## Getting started

You should use your company or hobbyist license to develop in this 
project. Be sure to read the markdown files, starting with README.md
and this file.

## Safety policy

SupXML is positioned as a memory-safe XML library — a Rust
replacement for libxml2 whose CVE history is full of buffer-overflow
and use-after-free bugs.  Every `unsafe` block we add is one chip out
of that pitch, so we hold a strict policy.

### Default: no unsafe

Most modules are marked with `#![forbid(unsafe_code)]`.  This is a
hard compile-time error — `unsafe { ... }` blocks in those files
won't even build.  If you're adding new code, write it in those
modules and you don't need to think about this section at all.

The forbidden modules cover everything except the parser engine:
the XSD validator, XPath evaluator, namespace resolver, serializer,
options/error/types, the public API surface, the tree types, and the
C-ABI compatibility shim.

### Where unsafe is allowed

Only inside:

- `crates/core/src/scanner.rs`        — raw cursor over byte streams
- `crates/core/src/xml_bytes_reader.rs` — reader that sits on top of Scanner
- `crates/core/src/reader.rs`         — `&str` wrapper over the bytes reader
- `crates/core/src/parser.rs`         — DOM-build entry points
- `crates/core/src/encoding.rs`       — UTF-8 / non-UTF-8 boundary

These files are the audited unsafe core.  Adding `unsafe` to a *new*
module requires removing the `#![forbid(unsafe_code)]` attribute from
that file, which should be visible in code review and discussed
explicitly in the PR.

### When you do write unsafe, the contract

Every `unsafe { ... }` block must be preceded by a comment with
**both** of these two parts:

1. **`SAFETY:`** — exactly *why* the operation is safe.  State the
   invariant a reviewer would need to verify.  "bounds checked above"
   is too vague; "the `if p >= end { return Eof }` above proves
   `p < end == bytes.len()` here" is the right level.
2. **`Why unsafe:`** — exactly *why* we're not using the safe
   equivalent.  This is almost always perf, and the comment should
   say so explicitly.  "dispatched on every event; safe `bytes[p]`
   adds a per-iteration bounds check that LLVM doesn't always elide"
   is the right level.

Example from `xml_bytes_reader.rs`:

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

The `Why unsafe:` clause is what stops `unsafe` from spreading.  If
you can't write a real perf justification, use safe Rust instead.

### Verification: Miri in CI

Every PR runs the test suite under [Miri](https://github.com/rust-lang/miri),
Rust's MIR interpreter.  Miri executes our tests one MIR instruction
at a time and detects every undefined-behaviour class — out-of-bounds
reads/writes, use-after-free, invalid pointer arithmetic, aliasing
violations, uninitialized-memory reads.  If a code change introduces
UB on any input the tests exercise, CI fails.

Miri is not a complete proof — it only sees inputs your tests run.
Combined with the audited surface (small, reviewed, `forbid` on
everything else) and the existing fuzz harness in `crates/core/fuzz`,
this gets us to roughly the same bar that `std`, `tokio`, and
`rustls` hold themselves to.

If you write a new `unsafe` block, please add a test that exercises
it directly so Miri sees the path.

### Running Miri locally

```bash
rustup toolchain install nightly
rustup +nightly component add miri
cargo +nightly miri setup
cargo +nightly miri test --workspace --exclude sup-xml-bench
```

`sup-xml-bench` is excluded because it links against libxml2 via
FFI for comparison benchmarks, which Miri can't interpret.

## Running the CLI locally

The CLI binary is `sup-xml`; its crate is `sup-xml-cli` (so the bin
target is `cargo run -p sup-xml-cli`).  It links the api crate with
the `xsd`, `xslt`, `html`, and `network-resolver` features turned
on, which is how the published binary ships.

Four ways to run it from a check-out, ordered from fastest-to-edit
to most-production-like:

```bash
# 1. cargo run — fastest edit/run loop, picks up source changes on
#    every invocation.  Use this while iterating on a subcommand.
#    Everything after `--` is forwarded to the CLI.
cargo run -p sup-xml-cli -- lint path/to/file.xml
cargo run -p sup-xml-cli -- xpath '/catalog/book[@id="b1"]' input.xml
cargo run -p sup-xml-cli -- validate --schema schema.xsd instance.xml
cargo run -p sup-xml-cli -- xslt --stylesheet style.xsl input.xml -o out.xml

# 2. cargo run --release — same forwarding, but optimised.  Use when
#    a debug-mode parse is too slow to be representative (XSD compile,
#    XSLT transforms over realistic-size documents).
cargo run --release -p sup-xml-cli -- stats large.xml
cargo run --release -p sup-xml-cli -- validate --schema schema.xsd big-corpus/*.xml

# 3. Build once, then call the binary directly.  Skips cargo's
#    up-to-date check on every invocation — handy in shell scripts
#    and tight loops.
cargo build --release -p sup-xml-cli
./target/release/sup-xml lint file.xml
./target/release/sup-xml --recover repair broken.xml -o clean.xml

# 4. cargo install — drops `sup-xml` into ~/.cargo/bin so you can
#    invoke it from anywhere without a working directory.  Use when
#    you want the local dev build to shadow a system install for
#    integration / behavioural testing.
cargo install --path crates/cli
sup-xml --help
sup-xml c14n --exclusive input.xml
```

### Discovering flags

`sup-xml --help` lists every subcommand and the global flags
(`--recover`, `--quiet`, `--allow-fs`, `--allow-host`, `--xinclude`,
`--html`, `--strict-html`, `--max-size`, `--buffer-size`, `--huge`,
`--timing`).  Each subcommand has its own per-flag help:

```bash
cargo run -p sup-xml-cli -- xpath --help
cargo run -p sup-xml-cli -- validate --help
cargo run -p sup-xml-cli -- xslt --help
```

### Common dev recipes

```bash
# Smoke-test a fresh change against the W3C XML conformance corpus.
# Exits 0 on every file the parser correctly rejects.
cargo run --release -p sup-xml-cli -- lint --quiet tests/w3c/xmltest/not-wf/sa/*.xml

# Validate an OOXML / SVG / SOAP document with debug build to get
# the most aggressive panic backtraces and assertion checks.
RUST_BACKTRACE=1 cargo run -p sup-xml-cli -- \
    validate --schema schema.xsd suspect.xml

# Drive XInclude resolution + canonicalization in one pipeline,
# useful when prepping a signed payload for XML-DSig.
cargo run --release -p sup-xml-cli -- \
    --xinclude --allow-fs $(pwd) c14n --exclusive input.xml

# Run the parser at the streaming layer (lint uses the streaming
# reader, not the DOM builder) — a faster sanity check than running
# the full test suite when iterating on `scanner.rs` /
# `xml_bytes_reader.rs`.
cargo run -p sup-xml-cli -- lint tests/assets/xmlts/xmltest/sa/*.xml
```

### Cross-checking against libxml2

Most CLI subcommands have a direct libxml2 / xmllint equivalent;
when a behavioural change is unclear, running both side-by-side is
the quickest way to confirm intent:

```bash
# Recovery-mode diff
xmllint --recover broken.xml
cargo run -p sup-xml-cli -- --recover repair broken.xml -o -

# XPath
xmllint --xpath '/catalog/book/@id' input.xml
cargo run -p sup-xml-cli -- xpath '/catalog/book/@id' input.xml

# XSD validation
xmllint --schema schema.xsd instance.xml --noout
cargo run -p sup-xml-cli -- validate --schema schema.xsd instance.xml
```

If you see a divergence, decide whether it's a SupXML bug, a libxml2
quirk we don't want to match (document the divergence in
`crates/core/src/xsd/SPEC_DIVERGENCES.md` or the equivalent
spec-divergence note for the affected component), or a known
spec-strict choice tracked in `COMPARISON.md`.
