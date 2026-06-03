# C ABI tests

Each `*.c` file in this directory is a self-contained C program that
exercises the `libsupxml2.so` (`libsup_xml_compat`) cdylib through its
public C API.  The Rust driver in `../tests/abi.rs` walks every `*.c`
file, compiles it against the workspace's built cdylib, runs the
resulting binary, and verifies the exit code and stdout.

## File layout

```
c-tests/
├── README.md              ← this file
├── common.h               ← shared CMocka-style helpers (TBD)
├── t-link-02.c            ← T-LINK-02: cdylib builds + loads
├── t-parse-01.c           ← T-PARSE-01: xmlReadMemory + walk (later)
├── ...
└── expected/
    ├── t-link-02.txt      ← golden stdout (empty for link-only tests)
    └── ...
```

## Test naming

Each test maps to one row in `thoughts/libxml2_abi_test_inventory.txt`
(the `T-AREA-NN` IDs).  The Rust driver uses the filename stem as the
test name in `cargo test` output.

## Adding a new test

1. Write `t-<area>-<nn>.c`.
2. Write `expected/t-<area>-<nn>.txt` with the golden stdout (empty
   file if the test prints nothing).
3. The Rust driver picks it up automatically — no Cargo.toml edits
   needed.

## What the harness does

Per `*.c` file at `cargo test --test abi` time:

1. Locate the cdylib produced by `cargo build -p sup-xml-compat`.
2. Invoke `cc` (system C compiler) to build the `.c` against it.
3. Run the binary with `DYLD_LIBRARY_PATH` (macOS) /
   `LD_LIBRARY_PATH` (Linux) pointing at the cdylib's directory.
4. Capture stdout, compare against `expected/<test>.txt` (whitespace-
   trimmed equality).
5. Assert exit code 0.

Failures surface as cargo test failures with the test name.

## Out of scope here

- Windows.  We don't ship `sup_xml_compat.dll` in v1.
- Linking against a vendored libxml2 for `_Static_assert` comparison.
  Add when Tier 1 tests need it.  Current layout assertions live in
  `crates/tree/src/dom.rs` as `const _: ()` blocks.
