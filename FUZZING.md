# Fuzzing sup-xml

Fuzz harness for the XSD compiler/validator and the XPath parser/evaluator.
Lives outside the workspace (so the regular `cargo build` doesn't compile it)
and uses its own toolchain because `cargo-fuzz` requires nightly Rust and a
LLVM-libfuzzer-capable build of `libfuzzer-sys`.

## Setup

```sh
rustup install nightly
cargo install cargo-fuzz   # one-time
```

## Targets

### `crates/core/fuzz/`

| Target              | What it stresses                                                              |
|---------------------|-------------------------------------------------------------------------------|
| `fuzz_xsd_compile`  | `Schema::compile_str` on arbitrary UTF-8 input — must never panic.            |
| `fuzz_xsd_validate` | `Schema::validate_str` against one of three pre-compiled schemas, on random input. The first input byte selects the schema (PO / key+keyref / choice); the rest is the instance document. |
| `fuzz_parse_xpath`  | `xpath::parse_xpath` on arbitrary UTF-8 input — lexer + recursive-descent parser must never panic. |
| `fuzz_xpath_eval`   | `xpath::eval` on parsed expressions against a fixed document — evaluator must never panic. |

### `crates/compat/fuzz/`

| Target              | What it stresses                                                              |
|---------------------|-------------------------------------------------------------------------------|
| `fuzz_build_uri`    | `xmlBuildURI`'s Rust core (`build_uri`) on arbitrary `(rel, base)` pairs. Input is UTF-8; a NUL byte (if any) splits rel from base. Calls the internal Rust function directly to avoid burning fuzzer cycles on CString round-trip noise. |

## Run

Always pass a **separate run-corpus directory** as the first corpus argument,
and the seed corpus as the second. libFuzzer writes newly discovered inputs
to the **first** corpus dir on the command line; if you omit the run-corpus
dir, it writes them into the seed corpus and pollutes the tree with thousands
of hash-named files.

```sh
cd crates/core

cargo +nightly fuzz run fuzz_xsd_compile \
  fuzz/corpus_run/fuzz_xsd_compile \
  fuzz/corpus/fuzz_xsd_compile

cargo +nightly fuzz run fuzz_xsd_validate \
  fuzz/corpus_run/fuzz_xsd_validate \
  fuzz/corpus/fuzz_xsd_validate

cargo +nightly fuzz run fuzz_parse_xpath \
  fuzz/corpus_run/fuzz_parse_xpath \
  fuzz/corpus/fuzz_parse_xpath

cargo +nightly fuzz run fuzz_xpath_eval \
  fuzz/corpus_run/fuzz_xpath_eval \
  fuzz/corpus/fuzz_xpath_eval
```

```sh
cd crates/compat

cargo +nightly fuzz run fuzz_build_uri \
  fuzz/corpus_run/fuzz_build_uri \
  fuzz/corpus/fuzz_build_uri
```

`corpus_run/` is gitignored — discard freely.

Bound runs by time:

```sh
cargo +nightly fuzz run fuzz_xsd_compile \
  fuzz/corpus_run/fuzz_xsd_compile \
  fuzz/corpus/fuzz_xsd_compile \
  -- -max_total_time=300
```

Or by iteration count:

```sh
cargo +nightly fuzz run fuzz_xsd_compile \
  fuzz/corpus_run/fuzz_xsd_compile \
  fuzz/corpus/fuzz_xsd_compile \
  -- -runs=1000000
```

## Seed corpus

Lives in `crates/core/fuzz/corpus/<target_name>/`. We ship hand-curated seeds
(real-world fixtures like `tests/assets/xml/xml_xsd_2.xml`, plus minimal
named cases like `001_root`, `002_wildcard`) so libFuzzer starts from valid
inputs and mutates outward — much more effective than random-byte bootstrapping.

Only commit curated seeds with descriptive names. Don't commit the 40-char
hex-named entries libFuzzer produces during a run; those belong in
`corpus_run/`.
