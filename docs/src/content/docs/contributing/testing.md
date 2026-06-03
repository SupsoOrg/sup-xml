---
title: Testing & Miri
description: cargo test-all, Miri setup, fuzz harness, W3C conformance suite.
---

## Run the full suite

```bash
cargo test-all
```

That's an alias for `cargo test --workspace --all-features` — exercises
every test in every crate under every optional feature (`xsd`, `xslt`,
`html`, `serde`, `tokio`, `network-resolver`, `full-encodings`, `c-abi`)
plus doctests, integration tests, and example builds.

Baseline: **all green**. A change that breaks the suite gets fixed in the
same PR.

## Miri

```bash
rustup toolchain install nightly
rustup +nightly component add miri
cargo +nightly miri setup
cargo +nightly miri test --workspace --exclude sup-xml-bench
```

Miri executes the suite one MIR instruction at a time, detecting every UB
class — out-of-bounds reads/writes, use-after-free, invalid pointer
arithmetic, aliasing violations, uninit reads. CI runs Miri on every PR.

`sup-xml-bench` is excluded because it links libxml2 via FFI, which Miri
can't interpret.

## Fuzzing

```bash
cd crates/core/fuzz
cargo +nightly fuzz run parse_str -- -max_total_time=600
```

Corpora are in `crates/core/fuzz/corpus/` — seeds drawn from W3C
conformance, common feeds, and CVE reproducers from libxml2 history.

## W3C conformance suite

The W3C suite is vendored at `tests/w3c/` (the `xmlconf-20130923`
snapshot). The harness lives at `crates/api/tests/w3c.rs` and runs as
a regular workspace test:

```bash
cargo test --release --test w3c -p sup-xml -- --nocapture
```

Expected output:

```text
W3C XML Conformance: 2274 passed, 0 failed, 0 xfail (allow-listed),
                     21 skipped (of 2295 total)
```

The failing-IDs allowlist lives in the harness itself
(`KNOWN_FAILING_IDS`, currently empty). A regression that drops the
count below 2274 fails CI; a test that *unexpectedly passes* off the
allowlist also fails, so the allowlist stays current.

## Fast feedback during development

```bash
cargo check-all     # compiles everything, skips tests — fast
cargo test -p sup-xml-core   # just the core crate
cargo test --doc             # just doctests
```
