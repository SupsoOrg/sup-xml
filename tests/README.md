# tests/

Test data and out-of-tree harnesses that don't live inside any one crate.
Each subfolder is consumed by Rust integration tests under
`crates/*/tests/`, benches under `crates/bench/benches/`, or by external
scripts that drive Python or shell tooling.

## Layout

| Path | What's there |
|---|---|
| [`abi-system/`](abi-system/) | End-to-end tests of the libxml2 ABI shim against real consumers — `lxml/` swaps the shim under Python's `lxml.etree`; `comparison/` runs each shim function side-by-side against the system libxml2 and asserts results match. |
| [`assets/`](assets/) | Real-world XML / HTML / XSD / XPath / XSLT inputs used by benches and integration tests.  Most subfolders fetch from upstream test suites (W3C XSTS, W3C XSLT 3.0, libxml2's XPath corpus) via a `fetch.sh` and are gitignored. |
| [`fixtures/`](fixtures/) | Small, hand-curated XML files for unit-level integration tests in `crates/api/tests/`.  `cve/` holds attack vectors (billion-laughs, etc.) that the parser must reject. |
| [`w3c/`](w3c/) | The W3C XML Conformance Test Suite — every `not-wf/**/*.xml` is engineered to trip one specific XML 1.0 well-formedness rule.  Driven by `crates/api/tests/w3c.rs`. |

## Conventions

- Anything large or fetched from upstream is gitignored.  Run the
  subfolder's `fetch.sh` once to populate.
- Hand-curated fixtures are committed and named descriptively (no
  hash-named files).
- See each subfolder's `README.md` for what its tests cover and how
  to run them.
