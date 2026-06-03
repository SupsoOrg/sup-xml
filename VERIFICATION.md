# Verification

This repo's primary defenses against bugs are `cargo test` (correctness)
and the fuzz targets under `crates/*/fuzz/` (panic-freedom on adversarial
input — see `FUZZING.md`).

Two heavier tools cover what those miss: **Kani** for exhaustive
panic-freedom proofs on bounded inputs, and **Miri** for undefined
behaviour in `unsafe` code. Neither is wired into CI; both run on
demand.

## Kani

[Kani](https://github.com/model-checking/kani) is a bounded model checker
that proves properties (no panics, no overflows, no OOB, no integer
arithmetic UB) by symbolic execution. Where fuzzing says "we didn't
find a bug in 1h," Kani says "no bug exists within these bounds."

### When it pays off

Small, pure functions with finite state where panic-freedom is the
contract. Currently used for the XPath axis-navigation helpers
(`following`, `following_siblings`, `preceding`, `preceding_siblings`)
in `crates/core/src/xpath/eval.rs` — they were the site of a slice OOB
the fuzzer found, and they fit Kani's sweet spot.

Bad fit: full parsers, validators, anything with large state. Use
fuzzing for those.

### Install

```sh
cargo install --locked kani-verifier
cargo kani setup     # one-time, downloads CBMC (~500MB)
```

### Run

```sh
cd crates/core
cargo kani
```

First invocation rebuilds the workspace through Kani's instrumented
toolchain — slow (10-30 min). Subsequent runs are faster.

### Where harnesses live

Inside the source file under `#[cfg(kani)] mod proofs { … }`, alongside
the implementation. Same convention as `#[cfg(test)] mod tests { … }`:
the harness and the function evolve together. Production builds never
see this code (`cargo build` / `cargo test` pass `--cfg kani=false`
implicitly).

A harness has three parts:

```rust
#[kani::proof]
#[kani::unwind(8)]                          // bound loops / recursion
fn following_never_panics() {
    let idx  = AnyIndex::any();             // symbolic doc shape
    let node: NodeId = kani::any();         // symbolic context node
    let _ = following(node, &idx);          // panic anywhere = proof fails
}
```

`kani::any::<T>()` produces a symbolic value of type `T`; Kani then
exhaustively explores every consistent assignment.

### Tuning bounds

Verification time grows fast with `MAX_NODES`, `MAX_CHILDREN`, and
`unwind`. The current settings in `eval.rs` (`MAX_NODES=4`,
`MAX_CHILDREN=3`, `unwind=5..8`) are large enough to expose the bug
class but can take 30-60 min per proof on first run. If a refactor
breaks a proof, prefer to first reproduce with a smaller bound to
keep the iteration loop fast.

### Adding a new harness

1. Write the symbolic stub for the trait the function consumes
   (see `AnyIndex` in `eval.rs` for the pattern — only stub the
   methods the function under test actually calls; trap the rest).
2. Pick the smallest `MAX_*` constants and `unwind` that still
   plausibly expose the bug shape you care about.
3. Add the `#[kani::proof]` harness in a `#[cfg(kani)] mod proofs`
   block at the bottom of the file.

## Miri

[Miri](https://github.com/rust-lang/miri) is an interpreter for Rust's
MIR that catches undefined behaviour — invalid pointer arithmetic,
data races, use-after-free, OOB unsafe indexing, uninit reads, stacked
borrows violations. It runs your existing tests under interpretation.

### When it pays off

Any crate with `unsafe` blocks. In this repo that's primarily
`crates/compat/` (libxml2 ABI shim — raw pointers, manual memory
management, `extern "C"` boundaries). Running the compat tests under
Miri would catch UB the regular `cargo test` silently passes over.

### Install

```sh
rustup +nightly component add miri
```

### Run

```sh
# All tests in a crate
cargo +nightly miri test -p sup-xml-compat

# Specific test
cargo +nightly miri test -p sup-xml-compat uri::tests::build_uri_dot_dot_segments

# Whole workspace (slow)
cargo +nightly miri test
```

Expect Miri to run ~10-100× slower than native — it's an interpreter,
not a compiler.

### What Miri can't run

- **Real FFI into C libraries.** Miri can interpret Rust calling
  `extern "C"` Rust functions, but not Rust calling into a C `.so` /
  `.dylib`. If a test loads a system library, Miri bails.
- **Some syscalls** — file I/O works (with `-Zmiri-disable-isolation`
  if you need real fs access); arbitrary network I/O does not.
- **`std::thread` is supported**; raw OS threading APIs aren't.

For this repo: pure-Rust tests (the URI normalizer, the XPath
evaluator, parser tests on in-memory XML) all run cleanly under Miri.
Tests that depend on linking against libxml2 itself for cross-check
do not.

### Suppressing known false positives

Stacked-borrows is the strictest aliasing model Miri checks; some
patterns common in FFI code (e.g. transmuting between `*const T` and
`*const U` for ABI-layout-compatible types) trip it. If you hit one
and have confirmed the pattern is sound, run with
`MIRIFLAGS=-Zmiri-tree-borrows cargo +nightly miri test ...` —
tree borrows is a looser aliasing model that accepts these patterns
while still catching real UB.

## Which tool for which problem

| Concern | Tool |
|---|---|
| Function panics on adversarial input | Fuzzing (broad), Kani (proof on bounded shapes) |
| Algorithmic blowup / DoS | Fuzzing (slow-units), step budgets at runtime |
| UB in `unsafe` blocks | Miri |
| Data races | Miri |
| Memory leaks in FFI bindings | Miri (with `-Zmiri-track-leaks`) |
| Functional correctness vs. spec | `cargo test`, W3C conformance suites |

Fuzz first, prove second. Kani is the heavy artillery for the
handful of functions where exhaustive verification is worth the
maintenance cost.
